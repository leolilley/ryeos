use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub type CommandRecord = ryeos_runtime::CommandDef;
pub const SECTION_NAME: &str = "commands";

pub struct CommandSection;

impl CommandSection {
    fn parse_command(&self, ctx: &NodeItemContext, body: &Value) -> Result<CommandRecord> {
        if body.get("name").is_some() {
            bail!(
                "command record '{}' declares path-owned structural field 'name' \
                 (command name is derived from path and must not be in node YAML)",
                ctx.id
            );
        }
        validate_command_path_id(&ctx.id)?;
        let mut record: CommandRecord =
            serde_json::from_value(body.clone()).context("failed to parse command record")?;
        record.name = ctx.id.clone();
        Ok(record)
    }
}

impl CompiledNodeConfigItem for CommandRecord {
    fn section_name(&self) -> &'static str {
        SECTION_NAME
    }

    fn admit(
        mut self: Box<Self>,
        target: &mut crate::node_config::loader::NodeConfigSnapshotBuilder,
        admission: &crate::node_config::loader::NodeConfigAdmission,
    ) -> anyhow::Result<()> {
        self.source_file = admission.source_file.clone();
        self.provenance = admission.command_provenance.clone();
        target.push_command(*self);
        Ok(())
    }
}

impl NodeConfigSection for CommandSection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn source_scope(&self) -> NodeConfigSourceScope {
        NodeConfigSourceScope::AppRootAndBundleRoots
    }

    fn load_spec(&self) -> SectionLoadSpec {
        SectionLoadSpec {
            phase: SectionLoadPhase::Full,
            traversal: SectionTraversal::Recursive,
            signer: SectionSignerPolicy::Trusted,
            cardinality: SectionCardinality::Any,
        }
    }

    fn parse(
        &self,
        ctx: &NodeItemContext,
        body: &Value,
    ) -> Result<Box<dyn CompiledNodeConfigItem>> {
        Ok(Box::new(self.parse_command(ctx, body)?))
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
            .parse_command(&ctx("demo/run"), &valid_body())
            .unwrap();
        assert_eq!(record.name, "demo/run");
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

    #[test]
    fn parse_rejects_path_owned_name_field() {
        let mut body = valid_body();
        body["name"] = serde_json::json!("demo/run");
        let error = CommandSection.parse(&ctx("demo/run"), &body).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("path-owned structural field 'name'")
        );
    }
}
