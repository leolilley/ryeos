use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ryeos_directive_core::{ProviderSetupModelProjection, ProviderSetupProjection};
use ryeos_node::{
    InitOperatorCeremony, InitOperatorProfile, InitOptions, InitPhase, InitProgress, InitReport,
    LifecycleController, LocalLifecycleEnv, PersistModelRouteOptions,
};
use zeroize::{Zeroize, Zeroizing};

use super::interaction::{
    Event, EventReader, Frame, InputAction, Key, ListItem, ListState, Pager, SecretInput,
    TerminalGuard, TextInput,
};
use super::onboarding_journal::{Journal, Phase};
use super::onboarding_spec::{self, ArtVariant, PageSpec};
use super::{Console, Row, StatusBanner, Tone};

const EVENT_TICK: Duration = Duration::from_millis(100);
const MINIMUM_WIDTH: u16 = 40;
const MINIMUM_HEIGHT: u16 = 12;

pub(crate) struct OnboardingOptions {
    pub init: InitOptions,
}

pub(crate) fn supported_geometry() -> bool {
    crossterm::terminal::size()
        .map(|(width, height)| width >= MINIMUM_WIDTH && height >= MINIMUM_HEIGHT)
        .unwrap_or(false)
}

pub(crate) async fn run(console: &Console, options: OnboardingOptions) -> Result<()> {
    let spec = onboarding_spec::load()?;
    let mut journal = Journal::load(&options.init.app_root)?;
    let reconciliation = journal.reconcile(&options.init.app_root)?;
    let already_initialized = journal.contains(Phase::CoreInitialized);
    let mut ui = FlowUi::enter(console)?;

    if !already_initialized {
        let welcome = page(&spec.pages, "welcome")?;
        if !ui
            .document(welcome, Some(&options.init), &reconciliation)
            .await?
        {
            return cancelled(ui, console, &journal, "No state was changed.");
        }
        journal.mark(Phase::WelcomeSeen);
        journal.save(&options.init.app_root)?;
        let identities = page(&spec.pages, "identity-explainer")?;
        if !ui.document(identities, Some(&options.init), &[]).await? {
            return cancelled(
                ui,
                console,
                &journal,
                "The welcome was acknowledged; no identity or node state was created.",
            );
        }
        let profile = collect_operator_profile(&mut ui).await?;
        let Some(profile) = profile else {
            return cancelled(
                ui,
                console,
                &journal,
                "No identity or node state was created.",
            );
        };
        let report = run_core_initialization(&mut ui, &options.init, profile).await?;
        let Some(report) = report else {
            journal.reconcile(&options.init.app_root)?;
            journal.save(&options.init.app_root)?;
            return cancelled(
                ui,
                console,
                &journal,
                "Initialization stopped at a safe phase boundary. Re-run `ryeos init` to resume; existing fingerprints will be preserved.",
            );
        };
        apply_init_report(&mut journal, &report);
        journal.save(&options.init.app_root)?;
    }

    if !ui.identity_reveal(&journal).await? {
        return cancelled(
            ui,
            console,
            &journal,
            "Core initialization is preserved; optional provider setup was not changed.",
        );
    }
    let setup = configure_provider(&mut ui, &options.init.app_root, &mut journal).await?;
    journal.mark(Phase::Complete);
    journal.save(&options.init.app_root)?;
    let open_tui = ui.completion(&journal, setup.as_ref()).await?;
    ui.finish()?;
    render_completion(console, &journal, setup.as_ref())?;
    if open_tui {
        let ryeos = std::env::current_exe().context("locate ryeos executable")?;
        let status = std::process::Command::new(ryeos)
            .arg("tui")
            .env("RYEOS_APP_ROOT", &options.init.app_root)
            .status()
            .context("launch ryeos tui")?;
        if !status.success() {
            bail!("ryeos tui exited with {status}");
        }
    }
    Ok(())
}

pub(crate) async fn run_setup(console: &Console, app_root: PathBuf) -> Result<()> {
    let mut journal = Journal::load(&app_root)?;
    journal.reconcile(&app_root)?;
    if !journal.contains(Phase::CoreInitialized) {
        bail!(
            "RyeOS initialization has no valid signed completion record\nRun: ryeos init --non-interactive"
        );
    }
    journal.save(&app_root)?;
    let mut ui = FlowUi::enter(console)?;
    let setup = configure_provider(&mut ui, &app_root, &mut journal).await?;
    ui.finish()?;
    if let Some(setup) = setup {
        let mut status = StatusBanner::new(Tone::Success, "SETUP COMPLETE");
        status.rows.push(Row::key_value("provider", setup.provider));
        status.rows.push(Row::key_value(
            "model",
            setup.model.unwrap_or_else(|| "not selected".to_string()),
        ));
        console.success(&status)?;
    } else {
        console.text("setup skipped; run `ryeos setup` whenever you are ready")?;
    }
    Ok(())
}

fn page<'a>(pages: &'a [PageSpec], id: &str) -> Result<&'a PageSpec> {
    pages
        .iter()
        .find(|page| page.id == id)
        .ok_or_else(|| anyhow!("embedded onboarding spec is missing page '{id}'"))
}

struct OperatorProfileInput {
    profile: InitOperatorProfile,
    contribution: Option<Zeroizing<Vec<u8>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("interactive initialization cancelled at a safe phase boundary")]
struct InteractiveInitCancelled;

async fn collect_operator_profile(ui: &mut FlowUi) -> Result<Option<OperatorProfileInput>> {
    let display_name = match ui
        .text_field(
            "Operator display name (optional)",
            "A local semantic label; the fingerprint remains your cryptographic identity.",
            80,
        )
        .await?
    {
        Some(value) => nonempty(value),
        None => return Ok(None),
    };
    let identity_statement = match ui
        .text_field(
            "Identity statement (optional)",
            "A short description of what this operator identity represents.",
            280,
        )
        .await?
    {
        Some(value) => nonempty(value),
        None => return Ok(None),
    };
    let contribution = match ui
        .secret_field(
            "Optional entropy contribution",
            "Not a password or recovery phrase. It is mixed with 256 bits from the OS and never stored verbatim.",
            1024,
            true,
        )
        .await?
    {
        Some(mut value) if !value.is_empty() => {
            let text = value.take_secret();
            let bytes = Zeroizing::new(text.as_bytes().to_vec());
            drop(text);
            Some(bytes)
        }
        Some(_) => None,
        None => return Ok(None),
    };
    if !ui
        .confirm(
            "Create operator and node identities?",
            "The selected app root will receive persistent cryptographic keys. Existing keys are never replaced.",
        )
        .await?
    {
        return Ok(None);
    }
    Ok(Some(OperatorProfileInput {
        profile: InitOperatorProfile {
            display_name,
            identity_statement,
        },
        contribution,
    }))
}

async fn run_core_initialization(
    ui: &mut FlowUi,
    options: &InitOptions,
    profile: OperatorProfileInput,
) -> Result<Option<InitReport>> {
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel::<InitProgress>();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let options = InitOptions {
        app_root: options.app_root.clone(),
        source_dir: options.source_dir.clone(),
        trust_files: options.trust_files.clone(),
        skip_preflight: options.skip_preflight,
    };
    let ceremony = InitOperatorCeremony {
        profile: profile.profile,
        entropy_contribution: profile.contribution,
    };
    let mut task = tokio::task::spawn_blocking(move || {
        ryeos_node::run_init_with_operator_ceremony(&options, ceremony, |progress| {
            if worker_cancelled.load(Ordering::Acquire) {
                return Err(anyhow!(InteractiveInitCancelled));
            }
            let _ = send.send(progress.clone());
            Ok(())
        })
    });
    let mut current = "preparing initialization".to_string();
    let mut detail = None;
    let mut terminated = false;
    loop {
        ui.progress(
            "Initialize RyeOS",
            &current,
            detail.as_deref(),
            cancelled.load(Ordering::Acquire),
        )?;
        tokio::select! {
            result = &mut task => {
                return match result {
                    Ok(Ok(_report)) if terminated => Err(terminal_terminated()),
                    Ok(Ok(_report)) if cancelled.load(Ordering::Acquire) => Ok(None),
                    Ok(Ok(report)) => Ok(Some(report)),
                    Ok(Err(_error)) if terminated => Err(terminal_terminated()),
                    Ok(Err(error)) if error.downcast_ref::<InteractiveInitCancelled>().is_some() => Ok(None),
                    Ok(Err(error)) => Err(error).context("core initialization failed"),
                    Err(error) => Err(anyhow!("initialization task failed: {error}")),
                };
            }
            progress = receive.recv() => {
                if let Some(progress) = progress {
                    current = init_phase_label(progress.phase).to_string();
                    detail = progress.detail;
                }
            }
            event = ui.events.next() => {
                match event? {
                    Event::Key(key) if key.is_control('c') || key.key == Key::Escape => {
                        cancelled.store(true, Ordering::Release);
                        current = "cancelling at the next safe phase boundary".to_string();
                    }
                    Event::Terminate => {
                        terminated = true;
                        cancelled.store(true, Ordering::Release);
                        current = "terminating at the next safe phase boundary".to_string();
                    }
                    Event::Resize { width, height } => ui.resize(width, height),
                    _ => {}
                }
            }
        }
    }
}

fn init_phase_label(phase: InitPhase) -> &'static str {
    match phase {
        InitPhase::PreparingLayout => "preparing node layout",
        InitPhase::InitializingIdentity => "initializing operator and node identities",
        InitPhase::PinningTrust => "pinning publisher trust",
        InitPhase::DiscoveringBundles => "discovering bundle sources",
        InitPhase::VerifyingBundles => "verifying bundle signatures",
        InitPhase::InstallingBundles => "installing verified bundles",
        InitPhase::InitializingVault => "initializing vault identity",
        InitPhase::Finalizing => "verifying initialized state",
    }
}

fn apply_init_report(journal: &mut Journal, report: &InitReport) {
    journal.operator_fingerprint = Some(report.user_key_fingerprint.clone());
    journal.node_fingerprint = Some(report.node_key_fingerprint.clone());
    journal.vault_fingerprint = Some(report.vault_pubkey_fingerprint.clone());
    journal.bundles_verified = Some(report.bundles_installed.len());
    journal.mark(Phase::OperatorCreated);
    journal.mark(Phase::CoreInitialized);
}

#[derive(Debug, Clone)]
struct SetupOutcome {
    provider: String,
    model: Option<String>,
    connected: bool,
}

async fn configure_provider(
    ui: &mut FlowUi,
    app_root: &Path,
    journal: &mut Journal,
) -> Result<Option<SetupOutcome>> {
    ui.progress("Provider setup", "starting local node", None, false)?;
    let env = LocalLifecycleEnv::load(Some(app_root.to_path_buf()))?;
    let controller = LifecycleController::from_env(env);
    let Some(started) = ui.await_cancellable(controller.start()).await? else {
        return Ok(None);
    };
    started.context("start node for provider setup")?;
    journal.mark(Phase::NodeStarted);
    journal.save(app_root)?;

    ui.progress(
        "Provider setup",
        "loading verified provider catalog",
        None,
        false,
    )?;
    let root = app_root.to_path_buf();
    let mut catalog_task =
        tokio::task::spawn_blocking(move || crate::setup::discover_verified_providers(&root));
    let Some(catalog_result) = ui.await_cancellable(&mut catalog_task).await? else {
        catalog_task.abort();
        return Ok(None);
    };
    let catalog =
        catalog_result.map_err(|error| anyhow!("provider discovery task failed: {error}"))??;
    if catalog.providers.is_empty() {
        ui.message(
            "No verified setup providers",
            "Initialization is complete. Install a bundle that declares provider setup metadata, then run `ryeos setup`.",
        )
        .await?;
        return Ok(None);
    }
    let Some(client_result) = ui
        .await_cancellable(crate::setup::LocalSetupClient::connect(app_root))
        .await?
    else {
        return Ok(None);
    };
    let client = client_result.context("connect setup client")?;
    let Some(keys_result) = ui.await_cancellable(client.vault_keys()).await? else {
        return Ok(None);
    };
    let mut configured_keys = keys_result.context("list configured credentials")?;

    'provider: loop {
        let Some(provider_index) = ui
            .provider_selector(&catalog.providers, &configured_keys, &catalog.warnings)
            .await?
        else {
            return Ok(None);
        };
        let provider = &catalog.providers[provider_index];
        journal.unmark(Phase::ProviderValidated);
        journal.unmark(Phase::ModelSelected);
        journal.model_name = None;
        journal.provider_id = Some(provider.provider_id.clone());
        journal.mark(Phase::ProviderSelected);
        journal.save(app_root)?;

        if let Some(credential) = &provider.credential {
            let configured = configured_keys
                .iter()
                .any(|key| key == &credential.secret_name);
            if configured
                && !ui
                    .confirm(
                        &format!("Replace {} credential?", provider.display_name),
                        "A credential already exists in the vault. Choose no to keep and validate it.",
                    )
                    .await?
            {
                journal.mark(Phase::CredentialStored);
            } else {
                let Some(mut secret) = ui
                    .secret_field(
                        &format!("{} {}", provider.display_name, credential.label),
                        provider
                            .help_url
                            .as_deref()
                            .unwrap_or("The value is sent only to the local node vault."),
                        16 * 1024,
                        false,
                    )
                    .await?
                else {
                    continue 'provider;
                };
                if secret.is_empty() {
                    ui.message("Credential required", "Enter a value or return to provider selection.")
                        .await?;
                    continue 'provider;
                }
                let value = secret.take_secret();
                client
                    .store_credential(&credential.secret_name, value.as_str())
                    .await
                    .context("store provider credential")?;
                drop(value);
                configured_keys.push(credential.secret_name.clone());
                configured_keys.sort();
                configured_keys.dedup();
                journal.mark(Phase::CredentialStored);
                journal.save(app_root)?;
            }
        }

        let model = ui.model_selector(provider).await?;
        if provider.validation.is_none() {
            let save_model = model.is_some()
                && ui
                    .confirm(
                        "Save this unvalidated model selection?",
                        "This provider does not declare a lightweight validation operation. The signed route can be saved, but connectivity has not been tested.",
                    )
                    .await?;
            if save_model {
                persist_model_selection(
                    ui,
                    app_root,
                    provider,
                    model.as_ref().expect("model present"),
                    journal,
                )
                .await?;
            }
            journal.unmark(Phase::ProviderValidated);
            journal.save(app_root)?;
            return Ok(Some(SetupOutcome {
                provider: provider.display_name.clone(),
                model: model.map(|model| model.display_name),
                connected: false,
            }));
        }

        loop {
            ui.progress(
                "Provider validation",
                &format!("checking {}", provider.display_name),
                provider.validation.as_ref().map(|validation| {
                    if validation.may_incur_cost {
                        "declared probe may incur provider cost"
                    } else {
                        "provider-native metadata probe"
                    }
                }),
                false,
            )?;
            let Some(validation_result) = ui
                .await_cancellable(
                    client.validate_provider(
                        provider,
                        model.as_ref().map(|model| model.name.as_str()),
                    ),
                )
                .await?
            else {
                return Ok(None);
            };
            match validation_result {
                Ok(_) => {
                    if let Some(model) = model.as_ref() {
                        persist_model_selection(ui, app_root, provider, model, journal).await?;
                    }
                    journal.mark(Phase::ProviderValidated);
                    journal.save(app_root)?;
                    return Ok(Some(SetupOutcome {
                        provider: provider.display_name.clone(),
                        model: model.map(|model| model.display_name),
                        connected: true,
                    }));
                }
                Err(error) => match ui.validation_error(&error.to_string()).await? {
                    ValidationChoice::Retry => continue,
                    ValidationChoice::Credential => {
                        let Some(credential) = &provider.credential else {
                            continue;
                        };
                        let Some(mut secret) = ui
                            .secret_field(
                                &format!("Replace {} {}", provider.display_name, credential.label),
                                "The previous value is not shown.",
                                16 * 1024,
                                false,
                            )
                            .await?
                        else {
                            continue;
                        };
                        let value = secret.take_secret();
                        client
                            .store_credential(&credential.secret_name, value.as_str())
                            .await?;
                        drop(value);
                    }
                    ValidationChoice::Provider => continue 'provider,
                    ValidationChoice::Skip => {
                        let save_model = model.is_some()
                            && ui
                                .confirm(
                                    "Save this unvalidated model selection?",
                                    "Validation did not succeed. Save the signed default route anyway?",
                                )
                                .await?;
                        if save_model {
                            persist_model_selection(
                                ui,
                                app_root,
                                provider,
                                model.as_ref().expect("model present"),
                                journal,
                            )
                            .await?;
                        }
                        journal.unmark(Phase::ProviderValidated);
                        journal.save(app_root)?;
                        return Ok(Some(SetupOutcome {
                            provider: provider.display_name.clone(),
                            model: model.map(|model| model.display_name),
                            connected: false,
                        }));
                    }
                },
            }
        }
    }
}

async fn persist_model_selection(
    ui: &mut FlowUi,
    app_root: &Path,
    provider: &ProviderSetupProjection,
    model: &ProviderSetupModelProjection,
    journal: &mut Journal,
) -> Result<()> {
    let root = app_root.to_path_buf();
    let mut verification_task =
        tokio::task::spawn_blocking(move || crate::setup::discover_verified_providers(&root));
    let Some(verified_result) = ui.await_cancellable(&mut verification_task).await? else {
        verification_task.abort();
        bail!("model route save cancelled");
    };
    let verified_catalog = verified_result
        .map_err(|error| anyhow!("provider re-verification task failed: {error}"))??;
    let verified_provider = verified_catalog
        .providers
        .iter()
        .find(|candidate| candidate.provider_id == provider.provider_id)
        .ok_or_else(|| anyhow!("selected provider is no longer present in the verified catalog"))?;
    let verified_model = verified_provider
        .models
        .iter()
        .find(|candidate| candidate.name == model.name)
        .ok_or_else(|| {
            anyhow!("selected model is no longer present in the verified provider definition")
        })?;
    let context_window = verified_model.context_window.ok_or_else(|| {
        anyhow!(
            "verified setup model '{}' does not declare context_window",
            verified_model.name
        )
    })?;
    let route = PersistModelRouteOptions {
        app_root: app_root.to_path_buf(),
        provider_id: verified_provider.provider_id.clone(),
        model_name: verified_model.name.clone(),
        context_window,
    };
    ui.progress(
        "Default model",
        "saving signed operator route at a safe mutation boundary",
        None,
        false,
    )?;
    tokio::task::spawn_blocking(move || ryeos_node::persist_default_model_route(&route))
        .await
        .map_err(|error| anyhow!("model route task failed: {error}"))??;
    journal.model_name = Some(verified_model.name.clone());
    journal.mark(Phase::ModelSelected);
    journal.save(app_root)
}

enum ValidationChoice {
    Retry,
    Credential,
    Provider,
    Skip,
}

struct FlowUi {
    guard: TerminalGuard,
    frame: Frame<std::io::Stdout>,
    events: EventReader,
    width: u16,
    height: u16,
}

impl FlowUi {
    fn enter(_console: &Console) -> Result<Self> {
        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
        let events = EventReader::new(EVENT_TICK)?;
        let guard = TerminalGuard::enter()?;
        Ok(Self {
            guard,
            frame: Frame::stdout(),
            events,
            width,
            height,
        })
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    async fn await_cancellable<F: std::future::Future>(
        &mut self,
        future: F,
    ) -> Result<Option<F::Output>> {
        tokio::pin!(future);
        loop {
            tokio::select! {
                output = &mut future => return Ok(Some(output)),
                event = self.events.next() => match event? {
                    Event::Key(key) if key.is_control('c') || key.key == Key::Escape => {
                        return Ok(None)
                    }
                    Event::Terminate => return Err(terminal_terminated()),
                    Event::Resize { width, height } => self.resize(width, height),
                    _ => {}
                }
            }
        }
    }

    fn finish(mut self) -> Result<()> {
        self.frame.clear()?;
        self.guard.restore()?;
        Ok(())
    }

    async fn document(
        &mut self,
        page: &PageSpec,
        init: Option<&InitOptions>,
        diagnostics: &[String],
    ) -> Result<bool> {
        loop {
            let mut lines = self.header(&page.title, page.art);
            lines.extend(wrap_indented(
                &page.body,
                usize::from(self.width).saturating_sub(6),
            ));
            if let Some(init) = init {
                lines.push(String::new());
                lines.push(format!(
                    "  app root      {}",
                    super::sanitize_terminal_inline(&init.app_root.display().to_string())
                ));
                lines.push(format!(
                    "  bundle source {}",
                    super::sanitize_terminal_inline(&init.source_dir.display().to_string())
                ));
            }
            for diagnostic in diagnostics {
                lines.extend(wrap_indented(
                    &format!("resume note: {diagnostic}"),
                    usize::from(self.width).saturating_sub(6),
                ));
            }
            lines.push(String::new());
            lines.push("  enter continue  ·  esc cancel".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) if key.key == Key::Enter => return Ok(true),
                Event::Key(key) if key.key == Key::Escape || key.is_control('c') => {
                    return Ok(false)
                }
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => self.resize(width, height),
                _ => {}
            }
        }
    }

    async fn text_field(&mut self, title: &str, help: &str, max: usize) -> Result<Option<String>> {
        let mut input = TextInput::new(max);
        loop {
            let mut lines = self.header(title, None);
            lines.extend(wrap_indented(
                help,
                usize::from(self.width).saturating_sub(6),
            ));
            lines.push(String::new());
            lines.push(format!("  > {}", input.value()));
            lines.push(String::new());
            lines.push("  enter accept  ·  esc cancel".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) => match input.handle_key(key) {
                    InputAction::Submit => return Ok(Some(input.value().to_string())),
                    InputAction::Cancel => return Ok(None),
                    _ if key.is_control('c') => return Ok(None),
                    _ => {}
                },
                Event::Paste(value) => {
                    input.paste(&value);
                }
                Event::Resize { width, height } => self.resize(width, height),
                Event::Terminate => return Err(terminal_terminated()),
                Event::Tick => {}
            }
        }
    }

    async fn secret_field(
        &mut self,
        title: &str,
        help: &str,
        max: usize,
        allow_empty: bool,
    ) -> Result<Option<SecretInput>> {
        let mut input = SecretInput::new(max);
        loop {
            let mut lines = self.header(title, None);
            lines.extend(wrap_indented(
                help,
                usize::from(self.width).saturating_sub(6),
            ));
            lines.push(String::new());
            lines.push(format!(
                "  > {}",
                if input.is_empty() { "" } else { "[entered]" }
            ));
            lines.push(String::new());
            lines.push("  input hidden  ·  enter accept  ·  esc cancel".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) => match input.handle_key(key) {
                    InputAction::Submit if allow_empty || !input.is_empty() => {
                        return Ok(Some(input))
                    }
                    InputAction::Cancel => return Ok(None),
                    _ if key.is_control('c') => return Ok(None),
                    _ => {}
                },
                Event::Paste(mut value) => {
                    input.paste(&value);
                    value.zeroize();
                }
                Event::Resize { width, height } => self.resize(width, height),
                Event::Terminate => return Err(terminal_terminated()),
                Event::Tick => {}
            }
        }
    }

    async fn confirm(&mut self, title: &str, body: &str) -> Result<bool> {
        loop {
            let mut lines = self.header(title, None);
            lines.extend(wrap_indented(
                body,
                usize::from(self.width).saturating_sub(6),
            ));
            lines.push(String::new());
            lines.push("  y confirm  ·  n/esc no".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) if key.key == Key::Char('y') || key.key == Key::Enter => {
                    return Ok(true)
                }
                Event::Key(key)
                    if matches!(key.key, Key::Char('n') | Key::Escape) || key.is_control('c') =>
                {
                    return Ok(false)
                }
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => self.resize(width, height),
                _ => {}
            }
        }
    }

    async fn identity_reveal(&mut self, journal: &Journal) -> Result<bool> {
        loop {
            let mut lines = self.header("Identity reveal", Some(ArtVariant::PrismCompact));
            push_wrapped_value(
                &mut lines,
                "operator",
                &fingerprint(journal.operator_fingerprint.as_deref()),
                self.width,
            );
            push_wrapped_value(
                &mut lines,
                "node",
                &fingerprint(journal.node_fingerprint.as_deref()),
                self.width,
            );
            push_wrapped_value(
                &mut lines,
                "vault",
                &fingerprint(journal.vault_fingerprint.as_deref()),
                self.width,
            );
            lines.push(String::new());
            lines.push("  These keys have distinct roles and are not interchangeable.".to_string());
            lines.push(String::new());
            lines.push("  enter continue to optional provider setup  ·  esc cancel".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) if key.key == Key::Enter => return Ok(true),
                Event::Key(key) if key.key == Key::Escape || key.is_control('c') => {
                    return Ok(false)
                }
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => self.resize(width, height),
                _ => {}
            }
        }
    }

    async fn provider_selector(
        &mut self,
        providers: &[ProviderSetupProjection],
        configured_keys: &[String],
        warnings: &[String],
    ) -> Result<Option<usize>> {
        let mut items = providers
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                let configured = provider
                    .credential
                    .as_ref()
                    .map(|credential| configured_keys.contains(&credential.secret_name))
                    .unwrap_or(true);
                ListItem::new(
                    &provider.provider_id,
                    format!("{} {}", provider.provider_id, provider.display_name),
                    (
                        index,
                        format!(
                            "{}{}",
                            if provider.recommended {
                                "recommended · "
                            } else {
                                ""
                            },
                            if configured {
                                "configured"
                            } else {
                                "credential needed"
                            }
                        ),
                    ),
                )
            })
            .collect::<Vec<_>>();
        items.push(ListItem::new(
            "skip",
            "skip later",
            (usize::MAX, "finish without a provider".to_string()),
        ));
        let mut list = ListState::new(items, usize::from(self.height).saturating_sub(8).max(1));
        loop {
            let mut lines = self.header("Connect a model provider", None);
            lines.push("  Options come from the verified installed provider catalog.".to_string());
            lines.push(String::new());
            for (visible, item) in list.visible_window() {
                let selected = Some(visible) == list.selected_visible_index();
                let provider_name = if item.value.0 == usize::MAX {
                    "Skip for now"
                } else {
                    &providers[item.value.0].display_name
                };
                let name_width = usize::from(self.width).saturating_sub(18).clamp(10, 28);
                let provider_name = super::clamp_visible(
                    &super::sanitize_terminal_inline(provider_name),
                    name_width,
                );
                let prefix = format!(
                    "  {} {provider_name:name_width$}  ",
                    if selected { ">" } else { " " },
                );
                let status = super::clamp_visible(
                    &super::sanitize_terminal_inline(&item.value.1),
                    usize::from(self.width)
                        .saturating_sub(super::visible_width(&prefix) + 1)
                        .max(1),
                );
                lines.push(format!("{prefix}{status}"));
            }
            if !warnings.is_empty() {
                lines.push(format!(
                    "  {} invalid provider definition(s) omitted",
                    warnings.len()
                ));
            }
            lines.push(String::new());
            lines.push("  enter select  ·  j/k move  ·  esc skip".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) if matches!(key.key, Key::Down | Key::Char('j')) => list.next(),
                Event::Key(key) if matches!(key.key, Key::Up | Key::Char('k')) => list.previous(),
                Event::Key(key) if key.key == Key::PageDown || key.is_control('d') => {
                    list.page_down()
                }
                Event::Key(key) if key.key == Key::PageUp || key.is_control('u') => list.page_up(),
                Event::Key(key) if key.key == Key::Enter => {
                    return Ok(list
                        .selected()
                        .and_then(|item| (item.value.0 != usize::MAX).then_some(item.value.0)));
                }
                Event::Key(key) if key.key == Key::Escape || key.is_control('c') => {
                    return Ok(None)
                }
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => {
                    self.resize(width, height);
                    list.set_viewport_rows(usize::from(height).saturating_sub(8).max(1));
                }
                _ => {}
            }
        }
    }

    async fn model_selector(
        &mut self,
        provider: &ProviderSetupProjection,
    ) -> Result<Option<ProviderSetupModelProjection>> {
        if provider.models.is_empty() {
            return Ok(None);
        }
        let mut items = provider
            .models
            .iter()
            .enumerate()
            .map(|(index, model)| {
                ListItem::new(
                    &model.name,
                    format!("{} {}", model.name, model.display_name),
                    index,
                )
            })
            .collect::<Vec<_>>();
        items.sort_by_key(|item| !provider.models[item.value].recommended);
        items.push(ListItem::new("skip", "skip no model", usize::MAX));
        let mut list = ListState::new(items, usize::from(self.height).saturating_sub(8).max(1));
        loop {
            let mut lines = self.header(&format!("Choose a {} model", provider.display_name), None);
            lines.push(String::new());
            for (visible, item) in list.visible_window() {
                let selected = Some(visible) == list.selected_visible_index();
                if item.value == usize::MAX {
                    lines.push(format!(
                        "  {} Skip model selection",
                        if selected { ">" } else { " " }
                    ));
                    continue;
                }
                let model = &provider.models[item.value];
                let context = model
                    .context_window
                    .map(|value| format!("{}k ctx", value / 1000))
                    .unwrap_or_else(|| "context not declared".to_string());
                let pricing = model
                    .pricing
                    .as_ref()
                    .map(|pricing| {
                        format!(
                            " · ${:.2}/${:.2} per M",
                            pricing.input_per_million, pricing.output_per_million
                        )
                    })
                    .unwrap_or_default();
                let name_width = usize::from(self.width).saturating_sub(20).clamp(10, 30);
                let model_name = super::clamp_visible(
                    &super::sanitize_terminal_inline(&model.display_name),
                    name_width,
                );
                let prefix = format!(
                    "  {} {model_name:name_width$}  ",
                    if selected { ">" } else { " " },
                );
                let metadata = format!(
                    "{context}{pricing}{}",
                    if model.recommended {
                        " · recommended"
                    } else {
                        ""
                    }
                );
                let metadata = super::clamp_visible(
                    &metadata,
                    usize::from(self.width)
                        .saturating_sub(super::visible_width(&prefix) + 1)
                        .max(1),
                );
                lines.push(format!("{prefix}{metadata}"));
            }
            lines.push(String::new());
            lines.push("  enter select  ·  j/k move  ·  esc skip".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) if matches!(key.key, Key::Down | Key::Char('j')) => list.next(),
                Event::Key(key) if matches!(key.key, Key::Up | Key::Char('k')) => list.previous(),
                Event::Key(key) if key.key == Key::Enter => {
                    return Ok(list.selected().and_then(|item| {
                        (item.value != usize::MAX).then(|| provider.models[item.value].clone())
                    }));
                }
                Event::Key(key) if key.key == Key::Escape || key.is_control('c') => {
                    return Ok(None)
                }
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => self.resize(width, height),
                _ => {}
            }
        }
    }

    async fn validation_error(&mut self, error: &str) -> Result<ValidationChoice> {
        let mut pager = super::interaction::Pager::new(
            error,
            usize::from(self.width).saturating_sub(6).max(1),
            usize::from(self.height).saturating_sub(8).max(1),
        );
        loop {
            let mut lines = self.header("Provider validation failed", None);
            lines.extend(
                pager
                    .visible_lines()
                    .iter()
                    .map(|line| format!("  {}", super::sanitize_terminal_inline(line))),
            );
            lines.push(String::new());
            lines.push(
                "  r retry · c change credential · p change provider · s skip · j/k scroll"
                    .to_string(),
            );
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) if key.is_control('c') => return Ok(ValidationChoice::Skip),
                Event::Key(key) if key.key == Key::Char('r') => return Ok(ValidationChoice::Retry),
                Event::Key(key) if key.key == Key::Char('c') => {
                    return Ok(ValidationChoice::Credential)
                }
                Event::Key(key) if key.key == Key::Char('p') => {
                    return Ok(ValidationChoice::Provider)
                }
                Event::Key(key) if key.key == Key::Char('s') || key.key == Key::Escape => {
                    return Ok(ValidationChoice::Skip)
                }
                Event::Key(key) if matches!(key.key, Key::Down | Key::Char('j')) => pager.down(),
                Event::Key(key) if matches!(key.key, Key::Up | Key::Char('k')) => pager.up(),
                Event::Key(key) if key.key == Key::PageDown || key.is_control('d') => {
                    pager.page_down()
                }
                Event::Key(key) if key.key == Key::PageUp || key.is_control('u') => pager.page_up(),
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => {
                    self.resize(width, height);
                    pager.set_geometry(
                        usize::from(width).saturating_sub(6).max(1),
                        usize::from(height).saturating_sub(8).max(1),
                    );
                }
                _ => {}
            }
        }
    }

    async fn completion(
        &mut self,
        journal: &Journal,
        setup: Option<&SetupOutcome>,
    ) -> Result<bool> {
        loop {
            let mut lines = self.header("RyeOS initialized", Some(ArtVariant::PrismCompact));
            push_wrapped_value(
                &mut lines,
                "operator",
                &fingerprint(journal.operator_fingerprint.as_deref()),
                self.width,
            );
            push_wrapped_value(
                &mut lines,
                "node",
                &fingerprint(journal.node_fingerprint.as_deref()),
                self.width,
            );
            push_wrapped_value(
                &mut lines,
                "vault",
                &fingerprint(journal.vault_fingerprint.as_deref()),
                self.width,
            );
            lines.push(format!(
                "  provider  {}",
                setup
                    .map(|setup| setup.provider.as_str())
                    .unwrap_or("not configured")
            ));
            lines.push(format!(
                "  bundles   {} verified",
                journal.bundles_verified.unwrap_or_default()
            ));
            lines.push(format!(
                "  model     {}",
                setup
                    .and_then(|setup| setup.model.as_deref())
                    .unwrap_or("not selected")
            ));
            lines.push(String::new());
            lines.push("  t open RyeOS TUI  ·  enter return to shell".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key) if key.key == Key::Char('t') => return Ok(true),
                Event::Key(key)
                    if key.key == Key::Enter || key.key == Key::Escape || key.is_control('c') =>
                {
                    return Ok(false)
                }
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => self.resize(width, height),
                _ => {}
            }
        }
    }

    async fn message(&mut self, title: &str, body: &str) -> Result<()> {
        loop {
            let mut lines = self.header(title, None);
            lines.extend(wrap_indented(
                body,
                usize::from(self.width).saturating_sub(6),
            ));
            lines.push(String::new());
            lines.push("  enter continue".to_string());
            self.frame.render(&lines)?;
            match self.events.next().await? {
                Event::Key(key)
                    if key.key == Key::Enter || key.key == Key::Escape || key.is_control('c') =>
                {
                    return Ok(())
                }
                Event::Terminate => return Err(terminal_terminated()),
                Event::Resize { width, height } => self.resize(width, height),
                _ => {}
            }
        }
    }

    fn progress(
        &mut self,
        title: &str,
        status: &str,
        detail: Option<&str>,
        cancelling: bool,
    ) -> Result<()> {
        let mut lines = self.header(title, None);
        lines.push(format!("  {} {status}", if cancelling { "▲" } else { "◆" }));
        if let Some(detail) = detail {
            lines.extend(wrap_indented(
                detail,
                usize::from(self.width).saturating_sub(6),
            ));
        }
        lines.push(String::new());
        lines.push(if cancelling {
            "  waiting for a safe mutation boundary…".to_string()
        } else {
            "  ctrl+c cancel at the next safe boundary".to_string()
        });
        self.frame.render(&lines)?;
        Ok(())
    }

    fn header(&self, title: &str, art: Option<ArtVariant>) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(art) = art {
            let source = if self.width < 60 || art == ArtVariant::PrismCompact {
                onboarding_spec::PRISM_COMPACT
            } else {
                onboarding_spec::PRISM_WIDE
            };
            lines.extend(source.lines().map(|line| format!("  {line}")));
        }
        lines.push(format!("  RYEOS  {title}"));
        lines.push(String::new());
        lines
    }
}

impl Drop for FlowUi {
    fn drop(&mut self) {
        let _ = self.frame.clear();
        let _ = self.guard.restore();
    }
}

fn cancelled(ui: FlowUi, console: &Console, journal: &Journal, detail: &str) -> Result<()> {
    ui.finish()?;
    let mut status = StatusBanner::new(Tone::Warning, "INITIALIZATION CANCELLED");
    status.detail = Some(detail.to_string());
    if let Some(fingerprint) = &journal.operator_fingerprint {
        status
            .rows
            .push(Row::key_value("operator preserved", fingerprint));
    }
    console.status(&status)?;
    Ok(())
}

fn render_completion(
    console: &Console,
    journal: &Journal,
    setup: Option<&SetupOutcome>,
) -> Result<()> {
    let mut status = StatusBanner::new(Tone::Success, "RYEOS INITIALIZED");
    status.rows = vec![
        Row::key_value(
            "operator",
            fingerprint(journal.operator_fingerprint.as_deref()),
        ),
        Row::key_value("node", fingerprint(journal.node_fingerprint.as_deref())),
        Row::key_value("vault", fingerprint(journal.vault_fingerprint.as_deref())),
        Row::key_value(
            "bundles",
            format!("{} verified", journal.bundles_verified.unwrap_or_default()),
        ),
        Row::key_value(
            "provider",
            setup
                .map(|setup| {
                    format!(
                        "{} · {}",
                        setup.provider,
                        if setup.connected {
                            "connected"
                        } else {
                            "not validated"
                        }
                    )
                })
                .unwrap_or_else(|| "not configured".to_string()),
        ),
        Row::key_value(
            "model",
            setup
                .and_then(|setup| setup.model.clone())
                .unwrap_or_else(|| "not selected".to_string()),
        ),
    ];
    console.success(&status)?;
    console.text("ryeos tui          open the RyeOS workspace\nryeos help         browse commands\nryeos node doctor  verify node health")?;
    Ok(())
}

fn fingerprint(value: Option<&str>) -> String {
    value.unwrap_or("not available").to_string()
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn push_wrapped_value(lines: &mut Vec<String>, label: &str, value: &str, width: u16) {
    let prefix = format!("  {label:<8}  ");
    let continuation = " ".repeat(super::visible_width(&prefix));
    let available = usize::from(width)
        .saturating_sub(super::visible_width(&prefix) + 1)
        .max(8);
    let pager = Pager::new(value, available, usize::MAX);
    for (index, part) in pager.visible_lines().iter().enumerate() {
        lines.push(if index == 0 {
            format!("{prefix}{part}")
        } else {
            format!("{continuation}{part}")
        });
    }
}

fn wrap_indented(value: &str, width: usize) -> Vec<String> {
    let sanitized = value
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                '�'
            }
        })
        .collect::<String>();
    Pager::new(&sanitized, width.max(8), usize::MAX)
        .visible_lines()
        .iter()
        .map(|line| format!("  {line}"))
        .collect()
}

fn terminal_terminated() -> anyhow::Error {
    anyhow!("interactive onboarding terminated by signal")
}
