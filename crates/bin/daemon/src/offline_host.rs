//! What the daemon supplies to signed offline operations applied at startup.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ryeos_app::execution_history_reset::SchedulerHistoryResetParticipant;
use ryeos_node::offline_operation::{BundleReplace, ImageSeed, OfflineHost, RemotePin};
use ryeos_node::provider_env::StartupProviderEnvironment;
use serde_json::Value;

use ryeosd::config::{Cli, Config, ConfigSources};

pub struct DaemonOfflineHost<'a> {
    image: Option<ImageSeed>,
    config_sources: ConfigSources,
    /// The provider environment this start took, when it could take one. A
    /// bundle replacement composes the node's configuration with it, as
    /// `run-service` does.
    provider_environment: Option<&'a StartupProviderEnvironment>,
}

impl<'a> DaemonOfflineHost<'a> {
    /// The image seed comes from the daemon's own command line, never from
    /// the process environment.
    pub fn from_cli(
        cli: &Cli,
        provider_environment: Option<&'a StartupProviderEnvironment>,
    ) -> Self {
        let image = match (&cli.image_seed_root, &cli.substrate_image_digest) {
            (Some(root), Some(digest)) => Some(ImageSeed {
                root: root.clone(),
                substrate_image_digest: digest.clone(),
            }),
            _ => None,
        };
        Self {
            image,
            config_sources: cli.to_sources(),
            provider_environment,
        }
    }
}

impl OfflineHost for DaemonOfflineHost<'_> {
    fn image_seed(&self) -> Option<ImageSeed> {
        self.image.clone()
    }

    fn scheduler_history_participant(
        &self,
        app_root: &Path,
    ) -> Result<Option<Box<dyn SchedulerHistoryResetParticipant>>> {
        let selected = ryeos_api::scheduler_selection::scheduler_selected_for_app_root(
            app_root,
            cfg!(feature = "scheduling"),
        )?;
        #[cfg(feature = "scheduling")]
        {
            Ok(selected.then(|| {
                Box::new(ryeos_scheduler_service::SchedulerHistoryReset::new())
                    as Box<dyn SchedulerHistoryResetParticipant>
            }))
        }
        #[cfg(not(feature = "scheduling"))]
        {
            // A build without the scheduler already refused a composition
            // that selects it, so there is no participant to take part.
            let _ = selected;
            Ok(None)
        }
    }

    fn pin_remote(&self, request: RemotePin<'_>) -> Result<Value> {
        pin_remote(request)
    }

    fn replace_bundle(&self, request: BundleReplace<'_>) -> Result<Value> {
        request
            .state_lock
            .ensure_protects_app_root(request.app_root)?;
        let provider_environment = self
            .provider_environment
            .context("this start has no provider environment, which composing the node's configuration for a bundle replacement needs")?;
        let config = Config::load(&self.config_sources)?;
        if config.app_root != request.app_root {
            bail!(
                "the daemon's configuration names another app root than the one being applied to"
            );
        }
        // The node's configuration composed exactly as `run-service` composes
        // it for a stopped-node `bundle/install`, so the replacement is
        // verified as a local `bundle install --replace` is.
        let (engine, _, _, isolation, _) =
            ryeosd::bootstrap::load_node_config_two_phase_standalone(&config, provider_environment)
                .context("compose the node's configuration to verify the replacement bundle")?;
        let validator = crate::prospective_node_config_validator(
            Arc::new(ryeos_ui::UiState::new()),
            Arc::clone(&engine),
        );
        let lease = request.state_lock.retain();
        ryeos_api::handlers::bundle_install::replace_from_archive(
            request.name,
            request.archive,
            request.preserve_runtime_artifacts,
            &ryeos_api::handlers::bundle_install::InstallAuthority {
                app_root: request.app_root,
                node_signing_key: request.node.signing_key(),
                node_trust_store: &engine.node_trust_store,
                prospective_validator: &validator,
                isolation,
                state_lock: &lease,
                // Nothing caches engines yet: the daemon has not started.
                generation_changed: &|_| {},
            },
        )
    }
}

/// Pin a remote from its descriptor in the node's own remotes file. Shared by
/// every host that links the remote configuration.
pub fn pin_remote(request: RemotePin<'_>) -> Result<Value> {
    request
        .state_lock
        .ensure_protects_app_root(request.app_root)?;
    let text = std::str::from_utf8(request.descriptor).context("the descriptor is not UTF-8")?;
    ryeos_api::remote::config::pin_remote(
        request.app_root,
        request.node.signing_key(),
        request.name,
        text,
        None,
    )
}
