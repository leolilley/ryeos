//! Composition-provided validation for prospective node-config snapshots.
//!
//! The generic app/API layers can load a prospective snapshot, but only the
//! daemon composition root knows every route extension contributed by sibling
//! crates such as `ryeos-ui`. This small typed extension keeps install
//! admission on the exact compiler surface used by the composed node without
//! introducing an API/UI dependency cycle.

use std::sync::Arc;

use anyhow::Result;

use crate::node_config::NodeConfigSnapshot;

type Validator = dyn Fn(&NodeConfigSnapshot) -> Result<()> + Send + Sync;

#[derive(Clone)]
pub struct ProspectiveNodeConfigValidator {
    validate: Arc<Validator>,
}

impl ProspectiveNodeConfigValidator {
    pub fn new(
        validate: impl Fn(&NodeConfigSnapshot) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            validate: Arc::new(validate),
        }
    }

    pub fn validate(&self, snapshot: &NodeConfigSnapshot) -> Result<()> {
        (self.validate)(snapshot)
    }
}
