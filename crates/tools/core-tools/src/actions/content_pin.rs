//! Complete signed external-content pins from current project bytes.
//!
//! This is an offline authoring transaction. It observes production manifests
//! but owns no CAS, thread, scheduler, or runtime handle.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use ryeos_engine::external_content::{
    DeclaringAuthority, ExternalContentDeclaration, ExternalContentKind, ExternalContentMode,
    ExternalContentRoot,
};
use ryeos_engine::kind_registry::{KindRegistry, KindSchema, validate_metadata_anchoring};
use ryeos_engine::parsers::ParserDispatcher;
use ryeos_engine::trust::TrustStore;
use ryeos_state::{
    DigestOnlyExternalContentSink, ExternalCapturePolicy, ExternalContentCaptureKind,
    LaunchCaptureBudget,
};

use super::sign;

#[derive(Debug, Clone, Default)]
pub struct ContentPinOptions {
    pub ids: Vec<String>,
    pub all: bool,
    pub update: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContentPinReport {
    pub item_ref: String,
    pub outcome: String,
    pub signer_fingerprint: String,
    pub durability_uncertain: bool,
    pub declarations: Vec<ContentPinDeclarationReport>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContentPinDeclarationReport {
    pub id: String,
    pub previous_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_digest: Option<String>,
    pub observed_digest: String,
    pub outcome: String,
}

struct AuthoringContext {
    _isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    kinds: KindRegistry,
    parsers: ParserDispatcher,
    trust_store: TrustStore,
    project_trust: Arc<PinnedProjectTrustContent>,
    app_root: PathBuf,
}

pub fn run_content_pin(
    item_ref: &str,
    project_path: &Path,
    options: &ContentPinOptions,
) -> Result<ContentPinReport> {
    if options.all && !options.ids.is_empty() {
        bail!("--all and --id are mutually exclusive");
    }
    let target = sign::parse_sign_target(item_ref)?;
    if item_ref.contains('*') || item_ref.contains('?') {
        bail!("content pin requires one exact canonical item ref");
    }
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
        .map_err(|error| anyhow!("malformed canonical ref `{item_ref}`: {error}"))?;
    if canonical.suffix.is_some() || canonical.to_string() != item_ref {
        bail!("content pin requires one exact canonical item ref");
    }

    let app_root = ryeos_engine::roots::app_root()
        .context("resolve app root for offline content authoring")?;
    let _state_lock = ryeos_app::state_lock::StateLock::acquire(
        &ryeos_app::state_lock::default_lock_path(&app_root),
    )
    .context("content pin is offline-only; stop the daemon before authoring")?;
    let project_root = lillux::PinnedDirectory::open(project_path)?
        .ok_or_else(|| anyhow!("project root is unavailable"))?;
    let _project_lock = project_root.lock_exclusive()?;
    let context = AuthoringContext::load(&project_root, app_root)?;
    let schema = context
        .kinds
        .get(&target.kind)
        .ok_or_else(|| anyhow!("unknown kind `{}`", target.kind))?;
    if schema.excludes_relative_path(Path::new(&target.bare_id)) {
        bail!("project item is beneath a kind-excluded auxiliary directory");
    }
    let (item_parent, item_name, item_relative, source_format, mut original_file) =
        open_item_source(&project_root, schema, &target.bare_id)?;
    let ai_root = project_path.join(ryeos_engine::AI_DIR);
    if crate::actions::runtime_owned::is_runtime_owned_file(
        &project_path.join(&item_relative),
        &ai_root,
    ) {
        bail!(
            "runtime-owned path is not authorable source: node runtime state and signing secrets are written by the daemon"
        );
    }
    let _authoring_lock = item_parent.lock_exclusive()?;
    ensure_exact_source_selection(&item_parent, schema, &target.bare_id, &item_name)?;
    item_parent.ensure_regular_entry_matches(OsStr::new(&item_name), Some(&original_file))?;
    let original_observation = lillux::observe_open_regular_file(&original_file)?;
    let original_full_mode = original_observation.full_permission_mode()?;
    if original_full_mode & 0o7000 != 0 {
        bail!("content pin refuses a source item with set-id or sticky permission bits");
    }
    let original_mode = original_observation.permission_mode()?;
    let original = lillux::read_open_regular_file_stable_bounded(
        &mut original_file,
        &original_observation,
        ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
    )
    .context("read stable project item")?;
    let original_text = std::str::from_utf8(&original).context("project item is not UTF-8")?;
    let envelope = &source_format.signature;
    let (body, existing_signature) = lillux::signature::strip_canonical_signature_with_envelope(
        original_text,
        &envelope.prefix,
        envelope.suffix.as_deref(),
        envelope.after_shebang,
    )?;

    let signing_key = sign::load_user_signing_key()?;
    let signer_fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
    if !context.trust_store.is_trusted(&signer_fingerprint) {
        bail!("operator signing key is not trusted for this project");
    }
    let parsed = context
        .parsers
        .dispatch(
            &source_format.parser,
            &body,
            Some(Path::new(&item_relative)),
            envelope,
        )
        .context("parse project item authoring draft")?;
    validate_metadata_anchoring(
        &parsed,
        &schema.extraction_rules,
        &schema.directory,
        &project_path.join(ryeos_engine::AI_DIR),
        &project_path.join(&item_relative),
    )
    .map_err(|error| anyhow!("path-anchoring validator refused item: {error}"))?;
    let contract = schema
        .execution
        .as_ref()
        .and_then(|execution| execution.external_content.as_ref());
    let declarations = ryeos_engine::external_content::declarations_from_authored_pin_draft(
        &parsed,
        contract,
        DeclaringAuthority::Project,
    )?;
    let selected = select_declarations(&declarations, options)?;

    let (ignore_parent, ignore_name) = open_ignore_policy(&context.app_root)?;
    let ignore_entry = ignore_parent
        .entry_no_follow(OsStr::new(&ignore_name))?
        .ok_or_else(|| anyhow!("node ingest-ignore policy is unavailable"))?;
    if ignore_entry.entry_type != lillux::PinnedEntryType::Regular {
        bail!("node ingest-ignore policy is not a regular file");
    }
    let mut ignore_file = ignore_parent
        .open_regular(OsStr::new(&ignore_name), false)?
        .ok_or_else(|| anyhow!("node ingest-ignore policy is unavailable"))?;
    let ignore_observation = lillux::observe_open_regular_file(&ignore_file)?;
    if !ignore_observation.matches_directory_entry(&ignore_entry) {
        bail!("node ingest-ignore policy changed before authoring");
    }
    let ignore_bytes = lillux::read_open_regular_file_stable_bounded(
        &mut ignore_file,
        &ignore_observation,
        1024 * 1024,
    )
    .context("read stable node ingest-ignore policy")?;
    let ignore_config: ryeos_state::ignore::IgnoreConfig =
        serde_yaml::from_slice(&ignore_bytes).context("parse node ingest-ignore policy")?;
    let ignore = ryeos_state::ignore::IgnoreMatcher::from_config(&ignore_config)
        .context("compile node ingest-ignore policy")?;

    let first = observe_all(
        &project_root,
        &declarations,
        &ignore,
        &item_relative,
        &selected,
    )?;
    let updates = build_updates(&declarations, &selected, &first, options)?;
    let edits = updates
        .iter()
        .flat_map(|(index, digest)| {
            let mode_pointer = format!("/external_content/{index}/mode");
            let digest_pointer = format!("/external_content/{index}/digest");
            [
                ryeos_handler_protocol::SourceScalarEdit {
                    expected: parsed.pointer(&mode_pointer).cloned(),
                    pointer: mode_pointer,
                    value: serde_json::json!("pinned"),
                },
                ryeos_handler_protocol::SourceScalarEdit {
                    expected: parsed.pointer(&digest_pointer).cloned(),
                    pointer: digest_pointer,
                    value: serde_json::json!(digest),
                },
            ]
        })
        .collect();
    let (edited_body, edited_value) = context
        .parsers
        .edit_source(
            &source_format.parser,
            &body,
            Some(Path::new(&item_relative)),
            edits,
        )
        .context("registered parser refused content-pin source editing")?;
    let parsed_final = context
        .parsers
        .dispatch(
            &source_format.parser,
            &edited_body,
            Some(Path::new(&item_relative)),
            envelope,
        )
        .context("registered parser refused the exact edited content-pin bytes")?;
    if parsed_final != edited_value {
        bail!(
            "registered parser returned an edited semantic value that disagrees with an independent parse of the exact bytes to be signed"
        );
    }
    validate_metadata_anchoring(
        &parsed_final,
        &schema.extraction_rules,
        &schema.directory,
        &project_path.join(ryeos_engine::AI_DIR),
        &project_path.join(&item_relative),
    )
    .map_err(|error| anyhow!("path-anchoring validator refused completed item: {error}"))?;
    sign::validate_authored_external_content(&parsed_final, schema, DeclaringAuthority::Project)?;

    let second = observe_all(
        &project_root,
        &declarations,
        &ignore,
        &item_relative,
        &selected,
    )?;
    if first != second {
        bail!("external content changed during pin authoring; item was not modified");
    }
    let current_ignore = lillux::read_open_regular_file_stable_bounded(
        &mut ignore_file,
        &ignore_observation,
        1024 * 1024,
    )?;
    if current_ignore != ignore_bytes {
        bail!("node ingest-ignore policy changed during pin authoring; item was not modified");
    }
    ignore_parent.ensure_entry_observation(&ignore_entry)?;
    ignore_parent.ensure_path_binding()?;

    let current_original = lillux::read_open_regular_file_stable_bounded(
        &mut original_file,
        &original_observation,
        ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
    )?;
    if current_original != original {
        bail!("project item changed during pin authoring; item was not modified");
    }
    context
        .project_trust
        .ensure_unchanged()
        .context("project trust changed during pin authoring; item was not modified")?;

    let signature_is_current = existing_signature.as_ref().is_some_and(|header| {
        lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            lillux::signature::content_to_sign(&body, envelope.after_shebang),
            &signing_key.verifying_key(),
            &signer_fingerprint,
        )
    });
    let already_exact = edited_body == body && signature_is_current;
    let signed = if already_exact {
        original_text.to_owned()
    } else {
        lillux::signature::sign_content_with_options(
            &edited_body,
            &signing_key,
            &envelope.prefix,
            envelope.suffix.as_deref(),
            envelope.after_shebang,
        )
    };
    if signed.len() as u64 > ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES {
        bail!("completed signed item exceeds the item source byte limit");
    }
    let mut durability_uncertain = false;
    if !already_exact {
        item_parent.ensure_path_binding()?;
        ensure_exact_source_selection(&item_parent, schema, &target.bare_id, &item_name)?;
        let expected_bytes = original.clone();
        let expected_observation = original_observation.clone();
        if let Err(error) = item_parent.replace_bytes_if_matches_atomic(
            OsStr::new(&item_name),
            Some(&original_file),
            move |incumbent| {
                let observed = lillux::observe_open_regular_file(incumbent)?;
                if !observed.matches_quarantined_incumbent(&expected_observation) {
                    bail!("project item metadata changed before publication");
                }
                let mut incumbent = incumbent.try_clone()?;
                let bytes = lillux::read_open_regular_file_stable_bounded(
                    &mut incumbent,
                    &observed,
                    ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
                )?;
                if bytes != expected_bytes {
                    bail!("project item bytes changed before publication");
                }
                Ok(())
            },
            signed.as_bytes(),
            original_mode,
        ) {
            if error.namespace_committed() {
                if let Err(sync_error) = item_parent.sync() {
                    durability_uncertain = true;
                    tracing::warn!(
                        error = %error,
                        sync_error = %sync_error,
                        "content pin committed but its parent durability could not be re-established"
                    );
                }
            } else {
                return Err(anyhow!(error));
            }
        }
    }
    if already_exact {
        item_parent.ensure_regular_entry_matches(OsStr::new(&item_name), Some(&original_file))?;
    }

    let declaration_reports = selected
        .iter()
        .map(|index| {
            let declaration = &declarations[*index];
            let observed = first
                .get(&declaration.id)
                .expect("selected locator was observed")
                .clone();
            ContentPinDeclarationReport {
                id: declaration.id.clone(),
                previous_mode: mode_label(declaration.mode).to_owned(),
                previous_digest: declaration.digest.clone(),
                observed_digest: observed.clone(),
                outcome: if already_exact {
                    "unchanged".to_owned()
                } else if declaration.mode == ExternalContentMode::Captured {
                    "pinned".to_owned()
                } else if declaration.digest.is_none() {
                    "completed".to_owned()
                } else if declaration.digest.as_deref() == Some(observed.as_str()) {
                    "resigned".to_owned()
                } else {
                    "updated".to_owned()
                },
            }
        })
        .collect();

    Ok(ContentPinReport {
        item_ref: item_ref.to_owned(),
        outcome: if already_exact { "unchanged" } else { "signed" }.to_owned(),
        signer_fingerprint,
        durability_uncertain,
        declarations: declaration_reports,
    })
}

impl AuthoringContext {
    fn load(project_root: &lillux::PinnedDirectory, app_root: PathBuf) -> Result<Self> {
        let isolation = ryeos_app::engine_init::load_locked_registered_isolation(&app_root)
            .context("load retained node isolation generation")?;
        let roots = isolation
            .registered_generation_bundle_roots()
            .context("retained isolation generation omitted bundle roots")?
            .to_vec();
        let node_trust = isolation
            .registered_generation_node_trust()
            .context("retained isolation generation omitted node trust")?;
        let project_trust = Arc::new(PinnedProjectTrustContent::new(project_root.try_clone()?));
        let trust_store = node_trust
            .with_project_keys_from_content(project_trust.as_ref())
            .context("load project trust")?;
        let kinds = sign::build_kind_registry(&roots, &trust_store)?;
        let parsers =
            sign::build_parser_dispatcher(&roots, &kinds, &trust_store, Arc::clone(&isolation))?;
        Ok(Self {
            _isolation: isolation,
            kinds,
            parsers,
            trust_store,
            project_trust,
            app_root,
        })
    }
}

/// Exact project-trust view used during one offline authoring transaction.
/// Path traversal and descriptor identity remain owned by Lillux; this layer
/// only adapts those facts to the engine's generic project-content contract.
struct PinnedProjectTrustContent {
    root: lillux::PinnedDirectory,
    observed: Mutex<BTreeMap<PathBuf, String>>,
}

impl PinnedProjectTrustContent {
    fn new(root: lillux::PinnedDirectory) -> Self {
        Self {
            root,
            observed: Mutex::new(BTreeMap::new()),
        }
    }

    fn engine_error(error: impl std::fmt::Display) -> ryeos_engine::error::EngineError {
        ryeos_engine::error::EngineError::Internal(error.to_string())
    }

    fn open_parent<'a>(&self, relative: &'a Path) -> Result<(lillux::PinnedDirectory, &'a OsStr)> {
        let name = relative
            .file_name()
            .ok_or_else(|| anyhow!("project trust path has no file name"))?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = if parent.as_os_str().is_empty() {
            self.root.try_clone()?
        } else {
            open_directory_relative(
                &self.root,
                parent
                    .to_str()
                    .ok_or_else(|| anyhow!("project trust path is not UTF-8"))?,
            )?
        };
        Ok((parent, name))
    }

    fn ensure_unchanged(&self) -> Result<()> {
        let expected = self
            .observed
            .lock()
            .map_err(|_| anyhow!("project trust observation lock was poisoned"))?
            .clone();
        let prefix = Path::new(ryeos_engine::AI_DIR).join(ryeos_engine::TRUST_KEYS_DIR);
        let Some(directory) = open_directory_relative_optional(
            &self.root,
            prefix
                .to_str()
                .ok_or_else(|| anyhow!("project trust prefix is not UTF-8"))?,
        )?
        else {
            if expected.is_empty() {
                self.root.ensure_path_binding()?;
                return Ok(());
            }
            bail!("project trust directory disappeared");
        };
        let files = directory.regular_files_bounded(ryeos_engine::trust::MAX_TRUST_DOCUMENTS)?;
        let mut current = BTreeMap::new();
        let mut total_bytes = 0_u64;
        for file in files {
            let relative = prefix.join(&file.name);
            if !ryeos_engine::trust::is_project_trust_document(Path::new(&file.name)) {
                continue;
            }
            let observation = lillux::observe_open_regular_file(&file.file)?;
            if observation.size() > ryeos_engine::trust::MAX_TRUST_DOCUMENT_BYTES {
                bail!("project trust document exceeds its byte bound");
            }
            total_bytes = total_bytes
                .checked_add(observation.size())
                .ok_or_else(|| anyhow!("project trust byte count overflow"))?;
            if total_bytes > ryeos_engine::trust::MAX_TRUST_DIRECTORY_BYTES {
                bail!("project trust documents exceed their aggregate byte bound");
            }
            let mut descriptor = file.file.try_clone()?;
            let (digest, _) =
                lillux::digest_open_regular_file_stable_exact(&mut descriptor, observation.size())?;
            current.insert(relative, digest);
        }
        directory.ensure_path_binding()?;
        self.root.ensure_path_binding()?;
        if current != expected {
            bail!("project trust documents changed during authoring");
        }
        Ok(())
    }
}

impl ryeos_engine::project_content::AuthoritativeProjectContent for PinnedProjectTrustContent {
    fn list_files(
        &self,
        prefix: &Path,
        recursive: bool,
        max_entries: usize,
    ) -> std::result::Result<
        Vec<ryeos_engine::project_content::ProjectContentEntry>,
        ryeos_engine::error::EngineError,
    > {
        if recursive {
            return Err(Self::engine_error(
                "project trust enumeration must be non-recursive",
            ));
        }
        let Some(directory) = open_directory_relative_optional(
            &self.root,
            prefix
                .to_str()
                .ok_or_else(|| Self::engine_error("project trust prefix is not UTF-8"))?,
        )
        .map_err(Self::engine_error)?
        else {
            return Ok(Vec::new());
        };
        let files = directory
            .regular_files_bounded(max_entries)
            .map_err(Self::engine_error)?;
        let mut observed = self.observed.lock().map_err(Self::engine_error)?;
        let mut entries = Vec::with_capacity(files.len());
        let mut total_bytes = 0_u64;
        for file in files {
            let relative_path = PathBuf::from(&file.name);
            if !ryeos_engine::trust::is_project_trust_document(&relative_path) {
                continue;
            }
            let observation =
                lillux::observe_open_regular_file(&file.file).map_err(Self::engine_error)?;
            if observation.size() > ryeos_engine::trust::MAX_TRUST_DOCUMENT_BYTES {
                return Err(Self::engine_error(
                    "project trust document exceeds its byte bound",
                ));
            }
            total_bytes = total_bytes
                .checked_add(observation.size())
                .ok_or_else(|| Self::engine_error("project trust byte count overflow"))?;
            if total_bytes > ryeos_engine::trust::MAX_TRUST_DIRECTORY_BYTES {
                return Err(Self::engine_error(
                    "project trust documents exceed their aggregate byte bound",
                ));
            }
            let mut descriptor = file.file.try_clone().map_err(Self::engine_error)?;
            let (content_hash, _) =
                lillux::digest_open_regular_file_stable_exact(&mut descriptor, observation.size())
                    .map_err(Self::engine_error)?;
            observed.insert(prefix.join(&relative_path), content_hash.clone());
            entries.push(ryeos_engine::project_content::ProjectContentEntry {
                relative_path,
                content_hash,
                size: observation.size(),
                normalized_mode: observation.portable_mode().map_err(Self::engine_error)?,
            });
        }
        directory
            .ensure_path_binding()
            .map_err(Self::engine_error)?;
        Ok(entries)
    }

    fn read_file(
        &self,
        relative_path: &Path,
        max_bytes: u64,
    ) -> std::result::Result<Option<Vec<u8>>, ryeos_engine::error::EngineError> {
        let (parent, name) = self
            .open_parent(relative_path)
            .map_err(Self::engine_error)?;
        let Some(mut file) = parent
            .open_regular(name, false)
            .map_err(Self::engine_error)?
        else {
            return Ok(None);
        };
        let observation = lillux::observe_open_regular_file(&file).map_err(Self::engine_error)?;
        let bytes =
            lillux::read_open_regular_file_stable_bounded(&mut file, &observation, max_bytes)
                .map_err(Self::engine_error)?;
        let digest = lillux::sha256_hex(&bytes);
        if self
            .observed
            .lock()
            .map_err(Self::engine_error)?
            .get(relative_path)
            .is_some_and(|expected| expected != &digest)
        {
            return Err(Self::engine_error(
                "project trust key changed during authoring",
            ));
        }
        parent.ensure_path_binding().map_err(Self::engine_error)?;
        Ok(Some(bytes))
    }

    fn validates_file(
        &self,
        relative_path: &Path,
        content_hash: &str,
    ) -> std::result::Result<bool, ryeos_engine::error::EngineError> {
        let (parent, name) = self
            .open_parent(relative_path)
            .map_err(Self::engine_error)?;
        let Some(file) = parent
            .open_regular(name, false)
            .map_err(Self::engine_error)?
        else {
            return Ok(false);
        };
        let observation = lillux::observe_open_regular_file(&file).map_err(Self::engine_error)?;
        if observation.size() > ryeos_engine::trust::MAX_TRUST_DOCUMENT_BYTES {
            return Err(Self::engine_error(
                "project trust document exceeds its byte bound",
            ));
        }
        let mut descriptor = file.try_clone().map_err(Self::engine_error)?;
        let (digest, _) =
            lillux::digest_open_regular_file_stable_exact(&mut descriptor, observation.size())
                .map_err(Self::engine_error)?;
        parent.ensure_path_binding().map_err(Self::engine_error)?;
        Ok(digest == content_hash)
    }

    fn validates_absence(
        &self,
        relative_path: &Path,
    ) -> std::result::Result<bool, ryeos_engine::error::EngineError> {
        let (parent, name) = self
            .open_parent(relative_path)
            .map_err(Self::engine_error)?;
        let absent = parent
            .entry_no_follow(name)
            .map_err(Self::engine_error)?
            .is_none();
        parent.ensure_path_binding().map_err(Self::engine_error)?;
        Ok(absent)
    }
}

fn open_item_source<'a>(
    project: &lillux::PinnedDirectory,
    schema: &'a KindSchema,
    bare_id: &str,
) -> Result<(
    lillux::PinnedDirectory,
    String,
    String,
    &'a ryeos_engine::kind_registry::ExtensionSpec,
    std::fs::File,
)> {
    let ai = project
        .open_child_directory(OsStr::new(ryeos_engine::AI_DIR))?
        .ok_or_else(|| anyhow!("project has no .ai directory"))?;
    let kind_root = open_directory_relative(&ai, &schema.directory)?;
    let (bare_parent, stem) = bare_id.rsplit_once('/').unwrap_or(("", bare_id));
    let parent = if bare_parent.is_empty() {
        kind_root
    } else {
        open_directory_relative(&kind_root, bare_parent)?
    };
    for spec in &schema.extensions {
        let name = format!("{stem}{}", spec.ext);
        let entry = parent.entry_no_follow(OsStr::new(&name))?;
        if !entry.is_some_and(|entry| entry.entry_type == lillux::PinnedEntryType::Regular) {
            continue;
        }
        if let Some(file) = parent.open_regular(OsStr::new(&name), false)? {
            let relative = format!(
                "{}/{}/{}{}",
                ryeos_engine::AI_DIR,
                schema.directory,
                bare_id,
                spec.ext
            );
            return Ok((parent, name, relative, spec, file));
        }
    }
    bail!("project item is unavailable")
}

fn ensure_exact_source_selection(
    parent: &lillux::PinnedDirectory,
    schema: &KindSchema,
    bare_id: &str,
    selected_name: &str,
) -> Result<()> {
    let stem = bare_id.rsplit_once('/').map_or(bare_id, |(_, stem)| stem);
    for spec in &schema.extensions {
        let name = format!("{stem}{}", spec.ext);
        let entry = parent.entry_no_follow(OsStr::new(&name))?;
        if name == selected_name {
            return match entry {
                Some(entry) if entry.entry_type == lillux::PinnedEntryType::Regular => Ok(()),
                _ => bail!("project item source selection changed during pin authoring"),
            };
        }
        if entry.is_some_and(|entry| entry.entry_type == lillux::PinnedEntryType::Regular) {
            bail!("a higher-priority project item source appeared during pin authoring");
        }
    }
    bail!("selected project item extension is no longer registered")
}

fn open_ignore_policy(app_root: &Path) -> Result<(lillux::PinnedDirectory, String)> {
    let root = lillux::PinnedDirectory::open(app_root)?
        .ok_or_else(|| anyhow!("node app root is unavailable"))?;
    let relative = ryeos_app::ignore::IGNORE_CONFIG_RELATIVE;
    let (parent, name) = relative.rsplit_once('/').expect("ignore path has a parent");
    Ok((open_directory_relative(&root, parent)?, name.to_owned()))
}

fn select_declarations(
    declarations: &[ExternalContentDeclaration],
    options: &ContentPinOptions,
) -> Result<BTreeSet<usize>> {
    if options.all && !options.ids.is_empty() {
        bail!("--all and --id are mutually exclusive");
    }
    let mut ids = BTreeSet::new();
    for id in &options.ids {
        if !ids.insert(id.as_str()) {
            bail!("external content id `{id}` was selected more than once");
        }
    }
    let eligible = declarations
        .iter()
        .enumerate()
        .filter(|(_, declaration)| declaration.locator.is_some())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if options.all {
        return Ok(eligible.into_iter().collect());
    }
    if !ids.is_empty() {
        let mut selected = BTreeSet::new();
        for id in ids {
            let (index, declaration) = declarations
                .iter()
                .enumerate()
                .find(|(_, declaration)| declaration.id == id)
                .ok_or_else(|| anyhow!("unknown external content id `{id}`"))?;
            if declaration.locator.is_none() {
                bail!("locator-free external content `{id}` cannot be learned");
            }
            selected.insert(index);
        }
        return Ok(selected);
    }
    match eligible.as_slice() {
        [only] => Ok(BTreeSet::from([*only])),
        [] => bail!("item has no locator-backed external content to pin"),
        _ => bail!("item has multiple locator-backed declarations; select --id or --all"),
    }
}

fn observe_all(
    project: &lillux::PinnedDirectory,
    declarations: &[ExternalContentDeclaration],
    ignore: &ryeos_state::ignore::IgnoreMatcher,
    item_relative: &str,
    selected: &BTreeSet<usize>,
) -> Result<BTreeMap<String, String>> {
    let mut budget = LaunchCaptureBudget::default();
    observe_all_with_budget(
        project,
        declarations,
        ignore,
        item_relative,
        selected,
        &mut budget,
    )
}

fn observe_all_with_budget(
    project: &lillux::PinnedDirectory,
    declarations: &[ExternalContentDeclaration],
    ignore: &ryeos_state::ignore::IgnoreMatcher,
    item_relative: &str,
    selected: &BTreeSet<usize>,
    budget: &mut LaunchCaptureBudget,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (index, declaration) in declarations.iter().enumerate() {
        let Some(locator) = declaration.locator.as_ref() else {
            continue;
        };
        if locator.root != ExternalContentRoot::ProjectFiles {
            bail!("project content pin may only observe the project_files root");
        }
        let policy = ExternalCapturePolicy::new(locator.path.clone(), ignore)?;
        let mut sink = DigestOnlyExternalContentSink;
        let manifest = ryeos_state::capture_external_content_at(
            project,
            &locator.path,
            match declaration.kind {
                ExternalContentKind::Tree => ExternalContentCaptureKind::Tree,
                ExternalContentKind::File => ExternalContentCaptureKind::File,
            },
            &declaration.exclude,
            &policy,
            budget,
            &mut sink,
        )?;
        if captured_manifest_contains_item(declaration, &manifest, item_relative)?
            && (selected.contains(&index) || declaration.mode == ExternalContentMode::Pinned)
        {
            bail!(
                "external content `{}` contains the item being rewritten; a pin cannot commit to its own signed bytes",
                declaration.id
            );
        }
        result.insert(
            declaration.id.clone(),
            ryeos_state::external_content_manifest_digest(&manifest)?,
        );
    }
    project.ensure_path_binding()?;
    Ok(result)
}

fn captured_manifest_contains_item(
    declaration: &ExternalContentDeclaration,
    manifest: &ryeos_state::objects::ExternalContentManifestObject,
    item_relative: &str,
) -> Result<bool> {
    let locator = declaration.locator.as_ref().expect("observed declaration");
    match declaration.kind {
        ExternalContentKind::File => Ok(locator.path == item_relative),
        ExternalContentKind::Tree => {
            let relative = item_relative
                .strip_prefix(&format!("{}/", locator.path))
                .or_else(|| (locator.path == ".").then_some(item_relative));
            Ok(relative.is_some_and(|relative| {
                manifest.entries.iter().any(|entry| entry.path == relative)
            }))
        }
    }
}

fn build_updates(
    declarations: &[ExternalContentDeclaration],
    selected: &BTreeSet<usize>,
    observed: &BTreeMap<String, String>,
    options: &ContentPinOptions,
) -> Result<BTreeMap<usize, String>> {
    let mut updates = BTreeMap::new();
    for index in selected {
        let declaration = &declarations[*index];
        let digest = observed
            .get(&declaration.id)
            .ok_or_else(|| anyhow!("selected external content was not observed"))?;
        if declaration.mode == ExternalContentMode::Pinned
            && declaration
                .digest
                .as_deref()
                .is_some_and(|old| old != digest)
            && !options.update
        {
            bail!(
                "external content `{}` is pinned to a different digest; pass --update to move it",
                declaration.id
            );
        }
        updates.insert(*index, digest.clone());
    }
    Ok(updates)
}

fn open_directory_relative(
    base: &lillux::PinnedDirectory,
    relative: &str,
) -> Result<lillux::PinnedDirectory> {
    open_directory_relative_optional(base, relative)?
        .ok_or_else(|| anyhow!("directory is unavailable"))
}

fn open_directory_relative_optional(
    base: &lillux::PinnedDirectory,
    relative: &str,
) -> Result<Option<lillux::PinnedDirectory>> {
    let mut current = base.try_clone()?;
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!("directory path is not canonical");
        }
        let Some(next) = current.open_child_directory(OsStr::new(segment))? else {
            return Ok(None);
        };
        current = next;
    }
    Ok(Some(current))
}

fn mode_label(mode: ExternalContentMode) -> &'static str {
    match mode {
        ExternalContentMode::Captured => "captured",
        ExternalContentMode::Pinned => "pinned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(
        id: &str,
        mode: ExternalContentMode,
        digest: Option<&str>,
    ) -> ExternalContentDeclaration {
        ExternalContentDeclaration {
            id: id.to_owned(),
            kind: ExternalContentKind::Tree,
            locator: Some(ryeos_engine::external_content::ExternalContentLocator {
                root: ExternalContentRoot::ProjectFiles,
                path: format!("vendor/{id}"),
            }),
            mode,
            digest: digest.map(str::to_owned),
            exclude: Vec::new(),
            metadata_hint: None,
            mount: format!("vendor/{id}"),
        }
    }

    #[test]
    fn selector_requires_intent_when_multiple_are_eligible() {
        let declarations = vec![
            declaration("one", ExternalContentMode::Captured, None),
            declaration("two", ExternalContentMode::Captured, None),
        ];
        assert!(select_declarations(&declarations, &ContentPinOptions::default()).is_err());
        let selected = select_declarations(
            &declarations,
            &ContentPinOptions {
                ids: vec!["two".to_owned()],
                ..ContentPinOptions::default()
            },
        )
        .unwrap();
        assert_eq!(selected, BTreeSet::from([1]));
    }

    #[test]
    fn selector_refuses_ambiguous_and_locator_free_requests() {
        let mut locator_free =
            declaration("banked", ExternalContentMode::Pinned, Some(&"a".repeat(64)));
        locator_free.locator = None;
        let declarations = vec![
            declaration("one", ExternalContentMode::Captured, None),
            locator_free,
        ];
        assert!(
            select_declarations(
                &declarations,
                &ContentPinOptions {
                    ids: vec!["missing".to_owned()],
                    ..ContentPinOptions::default()
                },
            )
            .is_err()
        );
        assert!(
            select_declarations(
                &declarations,
                &ContentPinOptions {
                    ids: vec!["one".to_owned(), "one".to_owned()],
                    ..ContentPinOptions::default()
                },
            )
            .is_err()
        );
        assert!(
            select_declarations(
                &declarations,
                &ContentPinOptions {
                    ids: vec!["banked".to_owned()],
                    ..ContentPinOptions::default()
                },
            )
            .is_err()
        );
        assert!(
            select_declarations(
                &declarations,
                &ContentPinOptions {
                    ids: vec!["one".to_owned()],
                    all: true,
                    update: false,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn authoring_observation_uses_one_budget_across_unselected_declarations() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("vendor/one")).unwrap();
        std::fs::create_dir_all(root.path().join("vendor/two")).unwrap();
        std::fs::write(root.path().join("vendor/one/a"), b"a").unwrap();
        std::fs::write(root.path().join("vendor/two/b"), b"b").unwrap();
        let project = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let ignore =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: vec![],
            })
            .unwrap();
        let declarations = vec![
            declaration("one", ExternalContentMode::Captured, None),
            declaration("two", ExternalContentMode::Captured, None),
        ];
        let mut budget = LaunchCaptureBudget::bounded(8, 1, 1024, 2048).unwrap();
        let error = observe_all_with_budget(
            &project,
            &declarations,
            &ignore,
            ".ai/graphs/subject.yaml",
            &BTreeSet::from([1]),
            &mut budget,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("maximum entry count"),
            "{error:#}"
        );
    }

    #[test]
    fn selected_self_containing_tree_is_refused_before_authoring() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".ai/graphs")).unwrap();
        std::fs::write(root.path().join(".ai/graphs/subject.yaml"), b"draft").unwrap();
        let project = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let ignore =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: vec![],
            })
            .unwrap();
        let mut declaration = declaration("self", ExternalContentMode::Captured, None);
        declaration.locator.as_mut().unwrap().path = ".ai".to_owned();
        let error = observe_all(
            &project,
            &[declaration],
            &ignore,
            ".ai/graphs/subject.yaml",
            &BTreeSet::from([0]),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot commit to its own signed bytes"),
            "{error:#}"
        );
    }

    #[test]
    fn existing_pin_moves_only_with_explicit_update_authority() {
        let declarations = vec![declaration(
            "one",
            ExternalContentMode::Pinned,
            Some(&"a".repeat(64)),
        )];
        let observed = BTreeMap::from([("one".to_owned(), "b".repeat(64))]);
        assert!(
            build_updates(
                &declarations,
                &BTreeSet::from([0]),
                &observed,
                &ContentPinOptions::default(),
            )
            .is_err()
        );
        assert!(
            build_updates(
                &declarations,
                &BTreeSet::from([0]),
                &observed,
                &ContentPinOptions {
                    update: true,
                    ..ContentPinOptions::default()
                },
            )
            .is_ok()
        );
    }

    #[test]
    fn project_trust_catalog_filters_and_bounds_before_hashing_bytes() {
        use ryeos_engine::project_content::AuthoritativeProjectContent as _;

        let root = tempfile::tempdir().unwrap();
        let trust = root.path().join(".ai/config/keys/trusted");
        std::fs::create_dir_all(&trust).unwrap();
        let ignored = std::fs::File::create(trust.join("huge.unsupported")).unwrap();
        ignored
            .set_len(ryeos_engine::trust::MAX_TRUST_DOCUMENT_BYTES + 1)
            .unwrap();
        std::fs::write(trust.join("valid.pub"), b"small").unwrap();
        let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let content = PinnedProjectTrustContent::new(pinned);
        let entries = content
            .list_files(
                Path::new(".ai/config/keys/trusted"),
                false,
                ryeos_engine::trust::MAX_TRUST_DOCUMENTS,
            )
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].relative_path, PathBuf::from("valid.pub"));

        let oversized = std::fs::File::create(trust.join("oversized.key")).unwrap();
        oversized
            .set_len(ryeos_engine::trust::MAX_TRUST_DOCUMENT_BYTES + 1)
            .unwrap();
        assert!(
            content
                .list_files(
                    Path::new(".ai/config/keys/trusted"),
                    false,
                    ryeos_engine::trust::MAX_TRUST_DOCUMENTS,
                )
                .is_err()
        );
    }
}
