use anyhow::{Context, Result};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, SectionRecord, SectionSourcePolicy};
use crate::routes::raw::RawRouteSpec;

pub struct RouteSection;

impl NodeConfigSection for RouteSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::EffectiveBundleRootsAndState
    }

    fn parse(&self, _name: &str, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let mut record: RawRouteSpec = serde_json::from_value::<RawRouteSpec>(body.clone())
            .context("failed to parse route record")?;

        if record.methods.is_empty() {
            anyhow::bail!("route '{}' has empty methods list", record.id);
        }

        record.section = "routes".to_string();
        record.source_file = std::path::PathBuf::new();

        Ok(Box::new(record))
    }
}

impl SectionRecord for RawRouteSpec {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_body() -> serde_json::Value {
        serde_json::json!({
            "id": "r1",
            "path": "/api/test",
            "methods": ["GET"],
            "auth": "none",
            "response": {
                "mode": "static",
                "status": 200,
                "content_type": "text/plain",
                "body_b64": "aGVsbG8="
            }
        })
    }

    #[test]
    fn valid_route_parses() {
        let section = RouteSection;
        let result = section.parse("r1", &valid_body());
        assert!(result.is_ok());
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let section = RouteSection;
        let mut body = valid_body();
        body["unknown_field"] = serde_json::json!("oops");
        let result = section.parse("r1", &body);
        assert!(result.is_err(), "expected error for unknown top-level field");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("unknown field"), "got: {msg}");
    }

    #[test]
    fn unknown_field_inside_limits_rejected() {
        let section = RouteSection;
        let mut body = valid_body();
        body["limits"] = serde_json::json!({
            "body_bytes_max": 1024,
            "timeout_ms": 30000,
            "concurrent_max": 10,
            "bogus_limit": 42
        });
        let result = section.parse("r1", &body);
        assert!(result.is_err(), "expected error for unknown field in limits");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("unknown field"), "got: {msg}");
    }

    #[test]
    fn unknown_field_inside_response_rejected() {
        let section = RouteSection;
        let mut body = valid_body();
        body["response"]["bogus_response_field"] = serde_json::json!("nope");
        let result = section.parse("r1", &body);
        assert!(result.is_err(), "expected error for unknown field in response");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("unknown field"), "got: {msg}");
    }
}
