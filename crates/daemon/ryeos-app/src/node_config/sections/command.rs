use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, SectionRecord, SectionSourcePolicy};

pub type CommandRecord = ryeos_runtime::CommandDef;

pub struct CommandSection;

impl NodeConfigSection for CommandSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::EffectiveBundleRootsAndState
    }

    fn parse(&self, name: &str, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let record: CommandRecord =
            serde_json::from_value(body.clone()).context("failed to parse command record")?;

        if record.name != name {
            bail!(
                "command record declares name '{}' but filename is '{}'",
                record.name,
                name
            );
        }
        if record.section != "commands" {
            bail!(
                "command record declares section '{}' but must be 'commands'",
                record.section
            );
        }
        if record.category != "commands" {
            bail!(
                "command record declares category '{}' but must be 'commands'",
                record.category
            );
        }

        Ok(Box::new(record))
    }
}

impl SectionRecord for CommandRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
