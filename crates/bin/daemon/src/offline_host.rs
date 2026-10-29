//! What the daemon supplies to signed offline operations applied at startup.

use std::path::Path;

use anyhow::Result;
use ryeos_app::execution_history_reset::SchedulerHistoryResetParticipant;
use ryeos_node::offline_operation::{ImageSeed, OfflineHost};

use ryeosd::config::Cli;

pub struct DaemonOfflineHost {
    image: Option<ImageSeed>,
}

impl DaemonOfflineHost {
    /// The image seed comes from the daemon's own command line, never from
    /// the process environment.
    pub fn from_cli(cli: &Cli) -> Self {
        let image = match (&cli.image_seed_root, &cli.substrate_image_digest) {
            (Some(root), Some(digest)) => Some(ImageSeed {
                root: root.clone(),
                substrate_image_digest: digest.clone(),
            }),
            _ => None,
        };
        Self { image }
    }
}

impl OfflineHost for DaemonOfflineHost {
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
}
