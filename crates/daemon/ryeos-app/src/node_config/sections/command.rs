use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy};

pub type CommandRecord = ryeos_runtime::CommandDef;

pub struct CommandSection;

impl NodeConfigSection for CommandSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::EffectiveBundleRootsAndState
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> Result<Box<dyn SectionRecord>> {
        if body.get("name").is_some() {
            bail!(
                "command record '{}' declares legacy structural field 'name' \
                 (command name is derived from path and must not be in node YAML)",
                ctx.id
            );
        }
        validate_command_path_id(&ctx.id)?;
        let mut record: CommandRecord =
            serde_json::from_value(body.clone()).context("failed to parse command record")?;
        record.name = ctx.id.clone();

        Ok(Box::new(record))
    }
}

impl SectionRecord for CommandRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn validate_command_path_id(id: &str) -> Result<()> {
    for segment in id.split('/') {
        if !is_valid_command_path_segment(segment) {
            bail!(
                "invalid command path segment '{}': must match ^[a-z][a-z0-9-]*$",
                segment
            );
        }
    }
    Ok(())
}

fn is_valid_command_path_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(id: &str) -> NodeItemContext {
        NodeItemContext {
            section: "commands".into(),
            id: id.into(),
            stem: id.rsplit('/').next().unwrap_or(id).into(),
            rel_path: format!("{id}.yaml").into(),
            source_file: format!("/tmp/{id}.yaml").into(),
            signer_fingerprint: "test".into(),
        }
    }

    fn valid_body() -> serde_json::Value {
        serde_json::json!({
            "tokens": ["demo"],
            "description": "Demo command",
            "dispatch": {
                "kind": "execute_ref",
                "execute": "tool:demo/run"
            }
        })
    }

    #[test]
    fn parse_derives_command_name_from_path_id() {
        let record = CommandSection
            .parse(&ctx("demo/run"), &valid_body())
            .unwrap();
        let command = record.as_any().downcast_ref::<CommandRecord>().unwrap();

        assert_eq!(command.name, "demo/run");
    }

    #[test]
    fn parse_rejects_invalid_path_segment() {
        let err = CommandSection
            .parse(&ctx("demo/bad_segment"), &valid_body())
            .unwrap_err();

        assert!(
            err.to_string().contains("invalid command path segment"),
            "got: {err:#}"
        );
    }
}
