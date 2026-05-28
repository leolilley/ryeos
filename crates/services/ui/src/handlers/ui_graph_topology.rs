//! `ui.graph.topology` — return the resolved `.ai/` topology for the web graph.
//!
//! This is intentionally a structured service result framed by the existing
//! `json` response mode. YAML remains the artifact/export format for saved
//! topology snapshots; the browser transport receives the same topology model
//! as native JavaScript data.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::contracts::{SignatureEnvelope, TrustClass};
use ryeos_engine::item_resolution::{
    enumerate_kind_refs, parse_signature_header, resolve_item_full,
};
use ryeos_engine::kind_registry::{DelegationVia, KindSchema};
use ryeos_engine::trust::verify_item_signature;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyGraph {
    pub version: String,
    pub kind: String,
    pub metadata: TopologyMetadata,
    pub nodes: Vec<TopologyNode>,
    pub edges: Vec<TopologyEdge>,
    pub views: TopologyViewHints,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyMetadata {
    pub generated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_surface: Option<String>,
    pub spaces: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(rename = "ref")]
    pub ref_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(rename = "virtual")]
    pub virtual_: bool,
    pub missing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<NodeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<TrustSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeStatus {
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composed: Option<bool>,
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustSummary {
    /// "trusted", "untrusted", "unsigned", or "unknown".
    pub class: String,
    /// Ed25519 signer fingerprint (SHA-256 hex of verifying key), if signed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyEdge {
    pub id: String,
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<EdgeSource>,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyViewHints {
    pub defaults: TopologyViewDefaults,
    pub filters: TopologyViewFilters,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyViewDefaults {
    pub group_by: String,
    pub color_by: String,
    pub label: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyViewFilters {
    pub kinds: Vec<String>,
    pub edge_types: Vec<String>,
}

struct GraphBuilder {
    nodes: BTreeMap<String, TopologyNode>,
    edges: BTreeMap<String, TopologyEdge>,
    kinds: BTreeSet<String>,
    edge_types: BTreeSet<String>,
}

impl GraphBuilder {
    fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            kinds: BTreeSet::new(),
            edge_types: BTreeSet::new(),
        }
    }

    fn add_node(&mut self, node: TopologyNode) {
        self.kinds.insert(node.kind.clone());
        self.nodes.insert(node.id.clone(), node);
    }

    fn add_ref_node(&mut self, ref_: &str, fallback_kind: &str) {
        if self.nodes.contains_key(ref_) {
            return;
        }
        let (kind, bare) =
            split_ref(ref_).unwrap_or_else(|| (fallback_kind.to_owned(), ref_.to_owned()));
        self.add_node(TopologyNode {
            id: ref_.to_owned(),
            kind: if ref_.starts_with("kind:") {
                "kind_schema".to_owned()
            } else {
                kind
            },
            label: label_for_bare_id(&bare),
            ref_: ref_.to_owned(),
            space: None,
            path: None,
            namespace: namespace_for_bare_id(&bare),
            virtual_: true,
            missing: true,
            status: None,
            trust: None,
        });
    }

    fn add_virtual_node(
        &mut self,
        id: impl Into<String>,
        kind: impl Into<String>,
        label: impl Into<String>,
        namespace: Option<String>,
    ) {
        let id = id.into();
        let kind = kind.into();
        let label = label.into();
        if let Some(existing) = self.nodes.get_mut(&id) {
            if existing.virtual_ && existing.missing {
                existing.kind = kind;
                existing.label = label;
                existing.namespace = namespace;
                existing.missing = false;
                existing.status = None;
                self.kinds.insert(existing.kind.clone());
            }
            return;
        }
        self.add_node(TopologyNode {
            id: id.clone(),
            kind,
            label,
            ref_: id,
            space: None,
            path: None,
            namespace,
            virtual_: true,
            missing: false,
            status: None,
            trust: None,
        });
    }

    fn add_missing_virtual_node(
        &mut self,
        id: impl Into<String>,
        kind: impl Into<String>,
        label: impl Into<String>,
        namespace: Option<String>,
    ) {
        let id = id.into();
        if self.nodes.contains_key(&id) {
            return;
        }
        self.add_node(TopologyNode {
            id: id.clone(),
            kind: kind.into(),
            label: label.into(),
            ref_: id,
            space: None,
            path: None,
            namespace,
            virtual_: true,
            missing: true,
            status: Some(NodeStatus {
                resolved: false,
                composed: None,
                executable: false,
            }),
            trust: None,
        });
    }

    fn add_edge(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        type_: impl Into<String>,
        label: impl Into<String>,
        source: Option<EdgeSource>,
        confidence: impl Into<String>,
    ) {
        let from = from.into();
        let to = to.into();
        let type_ = type_.into();
        let id = format!("{from}--{type_}--{to}");
        self.edge_types.insert(type_.clone());
        self.edges.entry(id.clone()).or_insert(TopologyEdge {
            id,
            from,
            to,
            type_,
            label: label.into(),
            source,
            confidence: confidence.into(),
        });
    }

    fn finish(self, metadata: TopologyMetadata) -> TopologyGraph {
        debug_assert!(self.edges.values().all(|edge| {
            self.nodes.contains_key(&edge.from) && self.nodes.contains_key(&edge.to)
        }));
        TopologyGraph {
            version: "1.0.0".into(),
            kind: "topology_graph".into(),
            metadata,
            nodes: self.nodes.into_values().collect(),
            edges: self.edges.into_values().collect(),
            views: TopologyViewHints {
                defaults: TopologyViewDefaults {
                    group_by: "kind".into(),
                    color_by: "kind".into(),
                    label: "label".into(),
                },
                filters: TopologyViewFilters {
                    kinds: self.kinds.into_iter().collect(),
                    edge_types: self.edge_types.into_iter().collect(),
                },
            },
        }
    }
}

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let session_id = session_id_from_context(&ctx).ok_or_else(|| {
        HandlerError::Forbidden("browser session required for topology graph".into())
    })?;

    let session = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

    let graph = build_topology(
        &state,
        session.project_root.clone(),
        Some(session.surface_ref),
    );
    serde_json::to_value(graph).map_err(Into::into)
}

fn build_topology(
    state: &AppState,
    project_root: Option<String>,
    root_surface: Option<String>,
) -> TopologyGraph {
    let mut builder = GraphBuilder::new();
    let project_path = project_root.as_ref().map(std::path::PathBuf::from);
    let roots = state.engine.resolution_roots(project_path);

    for kind in state.engine.kinds.kinds() {
        let Some(schema) = state.engine.kinds.get(kind) else {
            continue;
        };

        add_kind_schema_node_and_edges(&mut builder, kind, schema, state);

        for item_ref in enumerate_kind_refs(&roots, schema, kind) {
            let canonical = item_ref.to_string();
            let resolution = match resolve_item_full(&roots, schema, &item_ref) {
                Ok(r) => r,
                Err(_) => continue,
            };
            builder.add_node(TopologyNode {
                id: canonical.clone(),
                kind: kind.to_owned(),
                label: label_for_bare_id(&item_ref.bare_id),
                ref_: canonical.clone(),
                space: Some(space_to_string(resolution.winner_space)),
                path: Some(resolution.winner_path.display().to_string()),
                namespace: namespace_for_bare_id(&item_ref.bare_id),
                virtual_: false,
                missing: false,
                status: Some(NodeStatus {
                    resolved: true,
                    composed: None,
                    executable: schema.is_executable(),
                }),
                trust: classify_trust(
                    &resolution.winner_path,
                    schema
                        .spec_for(&resolution.matched_ext)
                        .map(|spec| &spec.signature),
                    &state.engine.trust_store,
                ),
            });

            add_item_edges(&mut builder, &canonical, &resolution.winner_path);
        }
    }

    builder.finish(TopologyMetadata {
        generated_at: lillux::time::iso8601_now(),
        project_root,
        root_surface,
        spaces: roots
            .ordered
            .iter()
            .map(|r| r.space.as_str().to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    })
}

fn add_kind_schema_node_and_edges(
    builder: &mut GraphBuilder,
    kind: &str,
    schema: &KindSchema,
    state: &AppState,
) {
    let kind_node_id = format!("kind:{kind}");
    builder.add_node(TopologyNode {
        id: kind_node_id.clone(),
        kind: "kind_schema".into(),
        label: kind.to_owned(),
        ref_: kind_node_id.clone(),
        space: Some("system".into()),
        path: None,
        namespace: Some("node/engine/kinds".into()),
        virtual_: true,
        missing: false,
        status: Some(NodeStatus {
            resolved: true,
            composed: Some(true),
            executable: schema.is_executable(),
        }),
        trust: None,
    });

    for ext in &schema.extensions {
        builder.add_ref_node(&ext.parser, "parser");
        builder.add_edge(
            kind_node_id.clone(),
            ext.parser.clone(),
            "uses_parser",
            "parser",
            Some(EdgeSource {
                field: Some("formats[].parser".into()),
                path: None,
            }),
            "structural",
        );
    }

    builder.add_ref_node(&schema.composer, "handler");
    builder.add_edge(
        kind_node_id.clone(),
        schema.composer.clone(),
        "uses_handler",
        "composer",
        Some(EdgeSource {
            field: Some("composer".into()),
            path: None,
        }),
        "structural",
    );

    if let Some(exec) = schema.execution() {
        if let Some(delegate) = &exec.delegate {
            let DelegationVia::RuntimeRegistry { serves_kind } = &delegate.via;
            let served_kind = serves_kind.as_deref().unwrap_or(kind);
            if let Ok(runtime) = state.engine.runtimes.lookup_for(served_kind) {
                let runtime_ref = runtime.canonical_ref.to_string();
                builder.add_ref_node(&runtime_ref, "runtime");
                builder.add_edge(
                    kind_node_id,
                    runtime_ref,
                    "uses_runtime",
                    "runtime",
                    Some(EdgeSource {
                        field: Some("execution.delegate".into()),
                        path: None,
                    }),
                    "structural",
                );
            }
        }
    }
}

fn add_item_edges(builder: &mut GraphBuilder, item_ref: &str, path: &std::path::Path) {
    let Some(raw) = read_item_body(path) else {
        return;
    };
    let value = read_structured_value(path, &raw);

    if let Some(parent) = value
        .as_ref()
        .and_then(|value| value.get("extends"))
        .and_then(|v| v.as_str())
    {
        builder.add_ref_node(parent, "item");
        builder.add_edge(
            item_ref.to_owned(),
            parent.to_owned(),
            "extends",
            "extends",
            Some(EdgeSource {
                field: Some("extends".into()),
                path: Some(path.display().to_string()),
            }),
            "structural",
        );
    }

    if let Some(value) = &value {
        add_context_edges(builder, item_ref, value, path);
        add_client_edges(builder, item_ref, value, path);
        add_executable_graph_edges(builder, item_ref, value, path);
    }

    add_execute_reference_edges(builder, item_ref, &raw, path);
}

fn add_client_edges(
    builder: &mut GraphBuilder,
    item_ref: &str,
    value: &serde_json::Value,
    path: &std::path::Path,
) {
    let Some(served_kind) = value
        .get("serves")
        .and_then(|serves| serves.get("kind"))
        .and_then(|v| v.as_str())
    else {
        return;
    };
    let kind_ref = format!("kind:{served_kind}");
    builder.add_ref_node(&kind_ref, "kind_schema");
    builder.add_edge(
        item_ref.to_owned(),
        kind_ref,
        "serves_kind",
        "serves",
        Some(EdgeSource {
            field: Some("serves.kind".into()),
            path: Some(path.display().to_string()),
        }),
        "structural",
    );
}

fn add_context_edges(
    builder: &mut GraphBuilder,
    item_ref: &str,
    value: &serde_json::Value,
    path: &std::path::Path,
) {
    let Some(context) = value.get("context") else {
        return;
    };

    let mut injected_refs = BTreeSet::new();
    let mut suppressed_refs = BTreeSet::new();
    if let Some(suppress) = context.get("suppress") {
        collect_string_refs(suppress, "knowledge:", &mut suppressed_refs);
    }
    if let Some(system) = context.get("system") {
        collect_string_refs(system, "knowledge:", &mut injected_refs);
    } else {
        collect_string_refs(context, "knowledge:", &mut injected_refs);
    }
    injected_refs.retain(|knowledge_ref| !suppressed_refs.contains(knowledge_ref));

    for knowledge_ref in injected_refs {
        builder.add_ref_node(&knowledge_ref, "knowledge");
        builder.add_edge(
            item_ref.to_owned(),
            knowledge_ref,
            "injects_context",
            "context",
            Some(EdgeSource {
                field: Some("context".into()),
                path: Some(path.display().to_string()),
            }),
            "structural",
        );
    }
    for knowledge_ref in suppressed_refs {
        builder.add_ref_node(&knowledge_ref, "knowledge");
        builder.add_edge(
            item_ref.to_owned(),
            knowledge_ref,
            "suppresses_context",
            "context",
            Some(EdgeSource {
                field: Some("context.suppress".into()),
                path: Some(path.display().to_string()),
            }),
            "structural",
        );
    }
}

fn add_executable_graph_edges(
    builder: &mut GraphBuilder,
    item_ref: &str,
    value: &serde_json::Value,
    path: &std::path::Path,
) {
    let Some(nodes) = value
        .get("config")
        .and_then(|config| config.get("nodes"))
        .and_then(|nodes| nodes.as_object())
    else {
        return;
    };

    for (node_name, node_value) in nodes {
        let graph_node_ref = format!("{item_ref}#node:{node_name}");
        builder.add_virtual_node(
            &graph_node_ref,
            "graph_node",
            node_name,
            Some(item_ref.to_owned()),
        );
        builder.add_edge(
            item_ref.to_owned(),
            graph_node_ref.clone(),
            "contains_node",
            "contains",
            Some(EdgeSource {
                field: Some(format!("config.nodes.{node_name}")),
                path: Some(path.display().to_string()),
            }),
            "structural",
        );

        if let Some(action_ref) = graph_action_ref(node_value) {
            let edge_type = match split_ref(&action_ref).map(|(kind, _)| kind) {
                Some(kind) if kind == "tool" => "calls_tool",
                Some(kind) if kind == "directive" || kind == "graph" => "spawns",
                _ => "references",
            };
            builder.add_ref_node(&action_ref, "item");
            builder.add_edge(
                graph_node_ref.clone(),
                action_ref,
                edge_type,
                "action",
                Some(EdgeSource {
                    field: Some(format!("config.nodes.{node_name}.action")),
                    path: Some(path.display().to_string()),
                }),
                "structural",
            );
        }

        for target in graph_next_targets(node_value) {
            let target_ref = format!("{item_ref}#node:{target}");
            if !builder.nodes.contains_key(&target_ref) {
                builder.add_missing_virtual_node(
                    &target_ref,
                    "graph_node",
                    &target,
                    Some(item_ref.to_owned()),
                );
            }
            builder.add_edge(
                graph_node_ref.clone(),
                target_ref,
                "flows_to",
                "next",
                Some(EdgeSource {
                    field: Some(format!("config.nodes.{node_name}.next")),
                    path: Some(path.display().to_string()),
                }),
                "structural",
            );
        }
    }
}

fn add_execute_reference_edges(
    builder: &mut GraphBuilder,
    item_ref: &str,
    raw: &str,
    path: &std::path::Path,
) {
    let refs = extract_execute_refs(raw);
    for (target_kind, target_ref) in refs {
        builder.add_ref_node(&target_ref, &target_kind);
        let edge_type = match target_kind.as_str() {
            "tool" => "calls_tool",
            "directive" | "graph" => "spawns",
            _ => "references",
        };
        builder.add_edge(
            item_ref.to_owned(),
            target_ref,
            edge_type,
            "execute",
            Some(EdgeSource {
                field: Some("body".into()),
                path: Some(path.display().to_string()),
            }),
            "heuristic",
        );
    }
}

fn read_item_body(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    Some(lillux::signature::strip_signature_lines(&raw))
}

fn read_structured_value(path: &std::path::Path, raw: &str) -> Option<serde_json::Value> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    if matches!(ext, "yaml" | "yml") {
        return serde_yaml::from_str(raw).ok();
    }
    if ext == "md" {
        return markdown_frontmatter(raw)
            .and_then(|frontmatter| serde_yaml::from_str(frontmatter).ok());
    }
    None
}

fn markdown_frontmatter(raw: &str) -> Option<&str> {
    let raw = raw.trim_start();
    let rest = raw.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn collect_string_refs(value: &serde_json::Value, prefix: &str, refs: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(s) => {
            if s.starts_with(prefix) {
                refs.insert(s.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_string_refs(item, prefix, refs);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_string_refs(item, prefix, refs);
            }
        }
        _ => {}
    }
}

fn graph_action_ref(node_value: &serde_json::Value) -> Option<String> {
    let action = node_value.get("action")?;
    if let Some(item_ref) = action.get("item_ref").and_then(|v| v.as_str()) {
        return Some(item_ref.to_owned());
    }
    let item_type = action.get("item_type").and_then(|v| v.as_str())?;
    let item_id = action.get("item_id").and_then(|v| v.as_str())?;
    Some(format!("{item_type}:{item_id}"))
}

fn graph_next_targets(node_value: &serde_json::Value) -> Vec<String> {
    let Some(next) = node_value.get("next") else {
        return Vec::new();
    };
    if let Some(to) = next.get("to").and_then(|v| v.as_str()) {
        return vec![to.to_owned()];
    }
    next.get("branches")
        .and_then(|branches| branches.as_array())
        .map(|branches| {
            branches
                .iter()
                .filter_map(|branch| branch.get("to").and_then(|v| v.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn extract_execute_refs(raw: &str) -> BTreeSet<(String, String)> {
    let mut refs = BTreeSet::new();
    for line in raw.lines() {
        if !(line.contains("rye_execute") || line.contains("<execute")) {
            continue;
        }
        let Some(item_type) = quoted_attr(line, "item_type") else {
            continue;
        };
        let Some(item_id) = quoted_attr(line, "item_id") else {
            continue;
        };
        refs.insert((item_type.clone(), format!("{item_type}:{item_id}")));
    }
    refs
}

fn quoted_attr(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_owned())
}

fn split_ref(ref_: &str) -> Option<(String, String)> {
    let parsed = CanonicalRef::parse(ref_).ok()?;
    Some((parsed.kind, parsed.bare_id))
}

fn label_for_bare_id(bare_id: &str) -> String {
    bare_id
        .rsplit_once('/')
        .map(|(_, label)| label)
        .unwrap_or(bare_id)
        .to_owned()
}

fn namespace_for_bare_id(bare_id: &str) -> Option<String> {
    bare_id
        .rsplit_once('/')
        .map(|(namespace, _)| namespace.to_owned())
}

fn space_to_string(space: ItemSpace) -> String {
    space.as_str().to_owned()
}

fn classify_trust(
    path: &std::path::Path,
    envelope: Option<&SignatureEnvelope>,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Option<TrustSummary> {
    let raw = std::fs::read_to_string(path).ok()?;
    let Some(envelope) = envelope else {
        return Some(TrustSummary {
            class: "unknown".into(),
            signer: None,
        });
    };

    let Some(header) = parse_signature_header(&raw, envelope) else {
        return Some(TrustSummary {
            class: "unsigned".into(),
            signer: None,
        });
    };

    let signer = Some(header.signer_fingerprint.clone());
    let class = match verify_item_signature(&raw, &header, envelope, trust_store) {
        Ok((TrustClass::Trusted, _)) => "trusted",
        Ok((TrustClass::Untrusted, _)) => "untrusted",
        Ok((TrustClass::Unsigned, _)) => "unsigned",
        Err(_) => "untrusted",
    };

    Some(TrustSummary {
        class: class.into(),
        signer,
    })
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/graph/topology",
    endpoint: "ui.graph.topology",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use ryeos_engine::trust::{compute_fingerprint, TrustStore, TrustedSigner};

    #[test]
    fn label_and_namespace_split_bare_ids() {
        assert_eq!(label_for_bare_id("ryeos/cockpit/graph"), "graph");
        assert_eq!(
            namespace_for_bare_id("ryeos/cockpit/graph"),
            Some("ryeos/cockpit".into())
        );
    }

    #[test]
    fn item_edges_extract_surface_extends_and_client_serves_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let surface_path = tmp.path().join("graph.yaml");
        std::fs::write(
            &surface_path,
            "extends: surface:ryeos/cockpit/base\nname: graph\n",
        )
        .unwrap();
        let client_path = tmp.path().join("web.yaml");
        std::fs::write(
            &client_path,
            "serves:\n  kind: surface\n  renderer: browser\n",
        )
        .unwrap();

        let mut builder = GraphBuilder::new();
        builder.add_ref_node("surface:ryeos/cockpit/graph", "surface");
        builder.add_ref_node("client:ryeos/web", "client");

        add_item_edges(&mut builder, "surface:ryeos/cockpit/graph", &surface_path);
        add_item_edges(&mut builder, "client:ryeos/web", &client_path);

        assert!(builder
            .edges
            .contains_key("surface:ryeos/cockpit/graph--extends--surface:ryeos/cockpit/base"));
        assert!(builder
            .edges
            .contains_key("client:ryeos/web--serves_kind--kind:surface"));
    }

    #[test]
    fn item_edges_extract_directive_context_and_execute_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let directive_path = tmp.path().join("review.md");
        std::fs::write(
            &directive_path,
            r#"---
context:
  system:
    - knowledge:project/coding-standards
  suppress:
    - knowledge:agent/core/Behavior
---

Use `rye_execute(item_type="tool", item_id="rye/code/git/git", parameters={})`.
Then `rye_execute(item_type="directive", item_id="rye/code/quality/review", parameters={})`.
"#,
        )
        .unwrap();

        let mut builder = GraphBuilder::new();
        builder.add_ref_node("directive:rye/code/quality/build", "directive");

        add_item_edges(
            &mut builder,
            "directive:rye/code/quality/build",
            &directive_path,
        );

        assert!(builder.edges.contains_key(
            "directive:rye/code/quality/build--injects_context--knowledge:project/coding-standards"
        ));
        assert!(builder.edges.contains_key(
            "directive:rye/code/quality/build--suppresses_context--knowledge:agent/core/Behavior"
        ));
        assert!(builder
            .edges
            .contains_key("directive:rye/code/quality/build--calls_tool--tool:rye/code/git/git"));
        assert!(builder.edges.contains_key(
            "directive:rye/code/quality/build--spawns--directive:rye/code/quality/review"
        ));
        assert_eq!(
            builder
                .edges
                .get("directive:rye/code/quality/build--calls_tool--tool:rye/code/git/git")
                .unwrap()
                .confidence,
            "heuristic"
        );
    }

    #[test]
    fn item_edges_extract_graph_nodes_actions_and_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("build.yaml");
        std::fs::write(
            &graph_path,
            r#"config:
  nodes:
    review:
      action:
        item_type: directive
        item_id: rye/code/quality/review
      next:
        to: gate
    gate:
      action:
        item_ref: tool:rye/code/quality/gate
"#,
        )
        .unwrap();

        let mut builder = GraphBuilder::new();
        builder.add_ref_node("graph:rye/code/build", "graph");

        add_item_edges(&mut builder, "graph:rye/code/build", &graph_path);

        assert!(builder
            .edges
            .contains_key("graph:rye/code/build--contains_node--graph:rye/code/build#node:review"));
        assert!(builder.edges.contains_key(
            "graph:rye/code/build#node:review--spawns--directive:rye/code/quality/review"
        ));
        assert!(builder.edges.contains_key(
            "graph:rye/code/build#node:review--flows_to--graph:rye/code/build#node:gate"
        ));
        assert!(builder.edges.contains_key(
            "graph:rye/code/build#node:gate--calls_tool--tool:rye/code/quality/gate"
        ));
    }

    #[test]
    fn missing_graph_flow_target_becomes_missing_virtual_item() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("build.yaml");
        std::fs::write(
            &graph_path,
            r#"config:
  nodes:
    review:
      next:
        to: missing_gate
"#,
        )
        .unwrap();

        let mut builder = GraphBuilder::new();
        builder.add_ref_node("graph:rye/code/build", "graph");
        add_item_edges(&mut builder, "graph:rye/code/build", &graph_path);

        let missing = builder
            .nodes
            .get("graph:rye/code/build#node:missing_gate")
            .expect("missing flow target should be represented");
        assert!(missing.virtual_);
        assert!(missing.missing);
        for edge in builder.edges.values() {
            assert!(builder.nodes.contains_key(&edge.from));
            assert!(builder.nodes.contains_key(&edge.to));
        }
    }

    #[test]
    fn defined_graph_flow_target_seen_later_is_not_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("build.yaml");
        std::fs::write(
            &graph_path,
            r#"config:
  nodes:
    alpha:
      next:
        to: zeta
    zeta:
      action:
        item_ref: tool:rye/code/quality/gate
"#,
        )
        .unwrap();

        let mut builder = GraphBuilder::new();
        builder.add_ref_node("graph:rye/code/build", "graph");
        add_item_edges(&mut builder, "graph:rye/code/build", &graph_path);

        let zeta = builder
            .nodes
            .get("graph:rye/code/build#node:zeta")
            .expect("defined flow target should exist");
        assert!(zeta.virtual_);
        assert!(
            !zeta.missing,
            "defined graph node must not be shown as missing"
        );
    }

    #[test]
    fn classify_trust_does_not_trust_tampered_signed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let item_path = tmp.path().join("item.yaml");
        let signing_key = SigningKey::from_bytes(&[11u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        let trust_store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fingerprint.clone(),
            verifying_key,
            label: Some("test".into()),
        }]);
        let envelope = SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };

        let body = "name: trusted\n";
        let signed = lillux::signature::sign_content_at(
            body,
            &signing_key,
            "#",
            None,
            "2026-05-27T00:00:00Z",
        );
        std::fs::write(&item_path, signed).unwrap();
        let trusted = classify_trust(&item_path, Some(&envelope), &trust_store)
            .expect("signed item should classify");
        assert_eq!(trusted.class, "trusted");
        assert_eq!(trusted.signer.as_deref(), Some(fingerprint.as_str()));

        let tampered = std::fs::read_to_string(&item_path)
            .unwrap()
            .replace(body, "name: tampered\n");
        std::fs::write(&item_path, tampered).unwrap();
        let tampered = classify_trust(&item_path, Some(&envelope), &trust_store)
            .expect("tampered signed item should classify");
        assert_ne!(tampered.class, "trusted");
    }
}
