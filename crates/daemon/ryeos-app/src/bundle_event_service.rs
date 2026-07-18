//! Daemon-enforced runtime API for bundle events.

use std::ffi::OsStr;
use std::io::Read as _;
use std::path::{Component, Path};
use std::sync::Arc;

use anyhow::Context;
use ryeos_bundle::manifest::BundleEventOperation;
use ryeos_bundle::runtime_authority::bundle_event_cap;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use ryeos_state::{
    BundleEventAppendRequest, BundleEventAppendResult, BundleEventCursor, BundleEventRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::callback_token::CallbackCapability;
use crate::state_store::{NewBundleEventAttachment, StateStore};

const MAX_BUNDLE_EVENT_ATTACHMENT_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventAttachmentSourceParams {
    pub name: String,
    pub source_path: String,
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventAppendParams {
    pub thread_id: String,
    pub event_kind: String,
    pub chain_id: String,
    pub event_type: String,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub expected_chain_head_hash: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub causation_id: Option<String>,
    #[serde(default)]
    pub attachments: Vec<BundleEventAttachmentSourceParams>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventReadChainParams {
    pub thread_id: String,
    pub event_kind: String,
    pub chain_id: String,
    #[serde(default)]
    pub cursor: Option<BundleEventCursor>,
    #[serde(default = "default_page_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventScanParams {
    pub thread_id: String,
    pub event_kind: String,
    #[serde(default)]
    pub cursor: Option<BundleEventCursor>,
    #[serde(default = "default_page_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventMaterializeAttachmentParams {
    pub thread_id: String,
    pub event_kind: String,
    pub event_hash: String,
    pub attachment_name: String,
    pub destination_path: String,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleEventAppendResponse {
    pub event_hash: String,
    pub chain_head_hash: String,
    pub event: ryeos_state::BundleEventObject,
    pub idempotent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleEventReadChainResponse {
    pub events: Vec<BundleEventRecord>,
    pub next_cursor: Option<BundleEventCursor>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleEventScanResponse {
    pub events: Vec<BundleEventRecord>,
    pub next_cursor: Option<BundleEventCursor>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleEventMaterializeAttachmentResponse {
    pub event_hash: String,
    pub attachment: ryeos_state::BundleEventAttachment,
    pub destination_path: String,
}

const DEFAULT_PAGE_LIMIT: usize = 16;
const MAX_PAGE_LIMIT: usize = 16;
const MAX_PAGE_SERIALIZED_BYTES: usize = 8 * 1024 * 1024;

pub struct BundleEventService;

impl BundleEventService {
    pub fn append(
        state_store: &Arc<StateStore>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: BundleEventAppendParams,
    ) -> anyhow::Result<BundleEventAppendResponse> {
        let effective_bundle_id = effective_bundle_id(cap)?;
        validate_bundle_identifiers(&effective_bundle_id, &params.event_kind)?;
        authorize_bundle_event(
            authorizer,
            &cap.effective_caps,
            &BundleEventOperation::Append,
            &effective_bundle_id,
            &params.event_kind,
        )?;
        let attachments = capture_attachment_sources(&cap.project_path, &params.attachments)?;
        let result = state_store.append_bundle_event_with_attachments(
            BundleEventAppendRequest {
                effective_bundle_id,
                bundle_id: None,
                event_kind: params.event_kind,
                chain_id: params.chain_id,
                event_type: params.event_type,
                schema_version: params.schema_version,
                payload: params.payload,
                expected_chain_head_hash: params.expected_chain_head_hash,
                idempotency_key: params.idempotency_key,
                correlation_id: params.correlation_id,
                causation_id: params.causation_id,
                attribution: attribution_for_callback(cap),
                attachments: vec![],
            },
            attachments,
        )?;
        Ok(result.into())
    }

    pub fn read_chain(
        state_store: &Arc<StateStore>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: BundleEventReadChainParams,
    ) -> anyhow::Result<BundleEventReadChainResponse> {
        let effective_bundle_id = effective_bundle_id(cap)?;
        validate_bundle_identifiers(&effective_bundle_id, &params.event_kind)?;
        validate_page_limit(params.limit)?;
        authorize_bundle_event(
            authorizer,
            &cap.effective_caps,
            &BundleEventOperation::Scan,
            &effective_bundle_id,
            &params.event_kind,
        )?;
        let page = state_store.read_bundle_event_chain_page(
            &effective_bundle_id,
            &params.event_kind,
            &params.chain_id,
            params.cursor.as_ref(),
            params.limit,
            MAX_PAGE_SERIALIZED_BYTES,
        )?;
        Ok(BundleEventReadChainResponse {
            events: page.records,
            next_cursor: page.next_cursor,
        })
    }

    pub fn scan(
        state_store: &Arc<StateStore>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: BundleEventScanParams,
    ) -> anyhow::Result<BundleEventScanResponse> {
        let effective_bundle_id = effective_bundle_id(cap)?;
        validate_bundle_identifiers(&effective_bundle_id, &params.event_kind)?;
        validate_page_limit(params.limit)?;
        authorize_bundle_event(
            authorizer,
            &cap.effective_caps,
            &BundleEventOperation::Scan,
            &effective_bundle_id,
            &params.event_kind,
        )?;
        let page = state_store.scan_bundle_events_page(
            &effective_bundle_id,
            &params.event_kind,
            params.cursor.as_ref(),
            params.limit,
            MAX_PAGE_SERIALIZED_BYTES,
        )?;
        Ok(BundleEventScanResponse {
            events: page.records,
            next_cursor: page.next_cursor,
        })
    }

    pub fn materialize_attachment(
        state_store: &Arc<StateStore>,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: BundleEventMaterializeAttachmentParams,
    ) -> anyhow::Result<BundleEventMaterializeAttachmentResponse> {
        let effective_bundle_id = effective_bundle_id(cap)?;
        validate_bundle_identifiers(&effective_bundle_id, &params.event_kind)?;
        ryeos_state::objects::validate_bundle_identifier(
            "attachment name",
            &params.attachment_name,
        )?;
        authorize_bundle_event(
            authorizer,
            &cap.effective_caps,
            &BundleEventOperation::Scan,
            &effective_bundle_id,
            &params.event_kind,
        )?;
        let (record, attachment, bytes) = state_store.read_bundle_event_attachment(
            &params.event_hash,
            &effective_bundle_id,
            &params.event_kind,
            &params.attachment_name,
        )?;
        materialize_project_relative_file(
            &cap.project_path,
            &params.destination_path,
            &bytes,
            params.replace,
        )?;
        Ok(BundleEventMaterializeAttachmentResponse {
            event_hash: record.event_hash,
            attachment,
            destination_path: params.destination_path,
        })
    }
}

fn capture_attachment_sources(
    project_root: &Path,
    sources: &[BundleEventAttachmentSourceParams],
) -> anyhow::Result<Vec<NewBundleEventAttachment>> {
    if sources.len() > ryeos_state::objects::MAX_BUNDLE_EVENT_ATTACHMENTS {
        anyhow::bail!(
            "bundle event has {} attachment sources (max {})",
            sources.len(),
            ryeos_state::objects::MAX_BUNDLE_EVENT_ATTACHMENTS
        );
    }
    let mut total_bytes = 0_u64;
    sources
        .iter()
        .map(|source| {
            ryeos_state::objects::validate_bundle_identifier("attachment name", &source.name)?;
            let mut file = open_project_relative_regular(project_root, &source.source_path)?;
            let mut bytes = Vec::new();
            file.by_ref()
                .take(ryeos_state::objects::MAX_BUNDLE_EVENT_ATTACHMENT_BYTES + 1)
                .read_to_end(&mut bytes)
                .with_context(|| {
                    format!("read bundle event attachment source {}", source.source_path)
                })?;
            let size_bytes = u64::try_from(bytes.len())
                .map_err(|_| anyhow::anyhow!("attachment size does not fit u64"))?;
            if size_bytes > ryeos_state::objects::MAX_BUNDLE_EVENT_ATTACHMENT_BYTES {
                anyhow::bail!(
                    "bundle event attachment '{}' is {} bytes (max {})",
                    source.name,
                    size_bytes,
                    ryeos_state::objects::MAX_BUNDLE_EVENT_ATTACHMENT_BYTES
                );
            }
            total_bytes = total_bytes
                .checked_add(size_bytes)
                .ok_or_else(|| anyhow::anyhow!("bundle event attachment size overflow"))?;
            if total_bytes > MAX_BUNDLE_EVENT_ATTACHMENT_TOTAL_BYTES {
                anyhow::bail!(
                    "bundle event attachments total {} bytes (max {})",
                    total_bytes,
                    MAX_BUNDLE_EVENT_ATTACHMENT_TOTAL_BYTES
                );
            }
            Ok(NewBundleEventAttachment {
                name: source.name.clone(),
                bytes,
                media_type: source.media_type.clone(),
            })
        })
        .collect()
}

fn relative_components(path: &str) -> anyhow::Result<Vec<&OsStr>> {
    if path.is_empty()
        || path.len() > 1024
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        anyhow::bail!("attachment path must be a non-empty canonical project-relative path");
    }
    let components = Path::new(path)
        .components()
        .map(|component| match component {
            Component::Normal(component) => Ok(component),
            _ => anyhow::bail!("attachment path must be contained within the project: {path}"),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if components.is_empty() {
        anyhow::bail!("attachment path must name a file");
    }
    Ok(components)
}

fn open_project_relative_regular(
    project_root: &Path,
    relative_path: &str,
) -> anyhow::Result<std::fs::File> {
    let components = relative_components(relative_path)?;
    let mut directory = lillux::PinnedDirectory::open(project_root)?
        .ok_or_else(|| anyhow::anyhow!("project root is absent: {}", project_root.display()))?;
    for component in &components[..components.len() - 1] {
        directory = directory
            .open_child_directory(component)?
            .ok_or_else(|| anyhow::anyhow!("attachment source directory is absent"))?;
    }
    directory
        .open_regular(components[components.len() - 1], false)?
        .ok_or_else(|| anyhow::anyhow!("attachment source is absent: {relative_path}"))
}

fn materialize_project_relative_file(
    project_root: &Path,
    relative_path: &str,
    bytes: &[u8],
    replace: bool,
) -> anyhow::Result<()> {
    let components = relative_components(relative_path)?;
    let mut directory = lillux::PinnedDirectory::open(project_root)?
        .ok_or_else(|| anyhow::anyhow!("project root is absent: {}", project_root.display()))?;
    for component in &components[..components.len() - 1] {
        directory = directory.open_or_create_child(component, 0o755)?;
    }
    let name = components[components.len() - 1];
    let existing = directory.open_regular(name, false)?;
    if existing.is_some() && !replace {
        anyhow::bail!("attachment destination already exists: {relative_path}");
    }
    directory.atomic_write_if_same(name, existing.as_ref(), bytes, 0o600)
}

impl From<BundleEventAppendResult> for BundleEventAppendResponse {
    fn from(result: BundleEventAppendResult) -> Self {
        Self {
            event_hash: result.event_hash,
            chain_head_hash: result.chain_head_hash,
            event: result.event,
            idempotent: result.idempotent,
        }
    }
}

fn default_schema_version() -> u32 {
    1
}

fn default_page_limit() -> usize {
    DEFAULT_PAGE_LIMIT
}

fn validate_page_limit(limit: usize) -> anyhow::Result<()> {
    if limit == 0 || limit > MAX_PAGE_LIMIT {
        anyhow::bail!(
            "bundle event page limit must be between 1 and {}",
            MAX_PAGE_LIMIT
        );
    }
    Ok(())
}

fn effective_bundle_id(cap: &CallbackCapability) -> anyhow::Result<String> {
    cap.effective_bundle_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("callback token has no effective_bundle_id"))
}

fn validate_bundle_identifiers(bundle_id: &str, event_kind: &str) -> anyhow::Result<()> {
    ryeos_state::objects::validate_bundle_identifier("bundle_id", bundle_id)?;
    ryeos_state::objects::validate_bundle_identifier("event_kind", event_kind)?;
    Ok(())
}

fn authorize_bundle_event(
    authorizer: &Authorizer,
    effective_caps: &[String],
    op: &BundleEventOperation,
    bundle_id: &str,
    event_kind: &str,
) -> anyhow::Result<()> {
    let required = bundle_event_cap(op, bundle_id, event_kind);
    authorizer
        .authorize(effective_caps, &AuthorizationPolicy::require(&required))
        .with_context(|| {
            format!(
                "missing required capability: {required} — bundle-event access is runtime \
                 authority: declare `runtime_authority.bundle_events:` for event kind \
                 '{event_kind}' in this bundle's `.ai/manifest.source.yaml` and sign it \
                 (`ryeos bundle publish`), then request it from the item under \
                 `requires.capabilities.manifest.runtime_authority`. It cannot be self-granted \
                 under `requires.capabilities.declared`."
            )
        })
}

fn attribution_for_callback(cap: &CallbackCapability) -> ryeos_state::BundleEventAttribution {
    ryeos_state::BundleEventAttribution {
        actor: None,
        tool: cap.item_ref.clone(),
        executor: None,
        site: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use crate::execution_provenance::ExecutionProvenance;

    fn cap(effective_caps: Vec<String>, effective_bundle_id: Option<&str>) -> CallbackCapability {
        let engine = Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            vec![],
        ));
        CallbackCapability {
            token: "cbt-test".into(),
            invocation_id: "inv-test".into(),
            thread_id: "T-test".into(),
            launch_owner: None,
            chain_root_id: "T-test".into(),
            project_path: PathBuf::from("/tmp/test"),
            expires_at: Instant::now() + Duration::from_secs(60),
            effective_caps,
            provenance: ExecutionProvenance::root_live_fs(PathBuf::from("/tmp/test"), engine),
            effective_bundle_id: effective_bundle_id.map(str::to_string),
            item_ref: Some("tool:example-bundle/send".into()),
            root_content_digest: "0".repeat(64),
            hard_limits: serde_json::Value::Null,
            depth: 0,
        }
    }

    #[test]
    fn authorizes_bundle_event_capability() {
        let authorizer = Authorizer::new();
        let cap = cap(
            vec!["ryeos.append.bundle-events.example-bundle/example_event".into()],
            Some("example-bundle"),
        );
        authorize_bundle_event(
            &authorizer,
            &cap.effective_caps,
            &BundleEventOperation::Append,
            "example-bundle",
            "example_event",
        )
        .unwrap();
    }

    #[test]
    fn validates_identifiers_before_capability_check() {
        let cap = cap(
            vec!["ryeos.scan.bundle-events.*".into()],
            Some("example-bundle"),
        );
        let bundle_id = effective_bundle_id(&cap).unwrap();
        let err = validate_bundle_identifiers(&bundle_id, "../bad").unwrap_err();
        assert!(format!("{err:#}").contains("unsafe character"));
    }

    #[test]
    fn attachment_paths_are_confined_and_materialized_atomically() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("models")).unwrap();
        std::fs::write(project.path().join("models/checkpoint.bin"), b"checkpoint").unwrap();
        let captured = capture_attachment_sources(
            project.path(),
            &[BundleEventAttachmentSourceParams {
                name: "checkpoint".to_string(),
                source_path: "models/checkpoint.bin".to_string(),
                media_type: Some("application/octet-stream".to_string()),
            }],
        )
        .unwrap();
        assert_eq!(captured[0].bytes, b"checkpoint");

        materialize_project_relative_file(
            project.path(),
            "models/restored/checkpoint.bin",
            b"restored",
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(project.path().join("models/restored/checkpoint.bin")).unwrap(),
            b"restored"
        );
        assert!(materialize_project_relative_file(
            project.path(),
            "models/restored/checkpoint.bin",
            b"replaced",
            false,
        )
        .is_err());
        materialize_project_relative_file(
            project.path(),
            "models/restored/checkpoint.bin",
            b"replaced",
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(project.path().join("models/restored/checkpoint.bin")).unwrap(),
            b"replaced"
        );
        assert!(open_project_relative_regular(project.path(), "../outside").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn attachment_capture_rejects_symlinked_components() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("checkpoint.bin"), b"outside").unwrap();
        symlink(outside.path(), project.path().join("models")).unwrap();
        let error =
            open_project_relative_regular(project.path(), "models/checkpoint.bin").unwrap_err();
        assert!(format!("{error:#}").contains("models"));
    }
}
