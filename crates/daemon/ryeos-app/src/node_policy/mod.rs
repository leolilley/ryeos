//! Exact node-owned semantic policy.
//!
//! Operational registrations and executable declarations belong to
//! [`crate::node_config`]. This module owns the distinct authority boundary:
//! one complete atomic node-signed generation under `.ai/node/policies/`.

use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde_json::Value;

pub mod generation;
pub mod sections;

/// Exact source identity supplied to a registered policy compiler.
pub struct NodePolicyContext {
    pub section: String,
    pub source_file: PathBuf,
    pub signer_fingerprint: String,
}

/// Implemented by every typed policy value. Its associated section name is
/// the only identity used by the generic registry and snapshot.
pub trait TypedNodePolicy: Any + Send + Sync + 'static {
    const SECTION_NAME: &'static str;
}

/// Object-safe erased policy value retained by the generic snapshot.
pub trait ErasedNodePolicy: Send + Sync {
    fn section_name(&self) -> &'static str;
    fn as_any(&self) -> &dyn Any;
}

impl<T: TypedNodePolicy> ErasedNodePolicy for T {
    fn section_name(&self) -> &'static str {
        T::SECTION_NAME
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Compiler for one mandatory member of the complete policy generation.
pub trait NodePolicySection: Send + Sync {
    fn name(&self) -> &'static str;
    fn parse(
        &self,
        context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>>;
}

/// Closed registry of every current node policy. Registration itself makes a
/// section mandatory; there is no optional cardinality or fallback mode.
pub struct NodePolicyTable {
    sections: Vec<Box<dyn NodePolicySection>>,
    indexes: BTreeMap<&'static str, usize>,
}

impl NodePolicyTable {
    pub fn new() -> Self {
        Self::from_sections(vec![
            Box::new(sections::accounting::NodeAccountingPolicySection),
            Box::new(sections::command_registration::CommandRegistrationPolicySection),
            Box::new(sections::execution::NodeExecutionPolicySection),
            Box::new(sections::external_content::ExternalContentImportPolicySection),
            Box::new(sections::hosted::HostedNodePolicySection),
            Box::new(sections::ingest_ignore::IngestIgnorePolicySection),
            Box::new(sections::isolation::IsolationPolicySection),
            Box::new(sections::maintenance::NodeMaintenancePolicySection),
            Box::new(sections::persistent_sessions::PersistentSessionPolicySection),
            Box::new(sections::thread_history::ThreadHistoryPolicySection),
        ])
        .expect("built-in node-policy table is valid")
    }

    fn from_sections(sections: Vec<Box<dyn NodePolicySection>>) -> anyhow::Result<Self> {
        let mut indexes = BTreeMap::new();
        for (index, section) in sections.iter().enumerate() {
            generation::validate_policy_name("node policy section", section.name())?;
            if indexes.insert(section.name(), index).is_some() {
                bail!("duplicate node-policy compiler `{}`", section.name());
            }
        }
        Ok(Self { sections, indexes })
    }

    pub fn get(&self, name: &str) -> Option<&dyn NodePolicySection> {
        self.indexes
            .get(name)
            .and_then(|index| self.sections.get(*index))
            .map(|section| section.as_ref())
    }

    pub fn sections(&self) -> impl Iterator<Item = &dyn NodePolicySection> {
        self.sections.iter().map(|section| section.as_ref())
    }
}

impl Default for NodePolicyTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Fully compiled semantic authority for one node generation.
#[derive(Clone)]
pub struct NodePolicySnapshot {
    records: BTreeMap<&'static str, Arc<dyn ErasedNodePolicy>>,
    source_files: BTreeMap<&'static str, PathBuf>,
    generation_digest: String,
}

impl std::fmt::Debug for NodePolicySnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NodePolicySnapshot")
            .field("sections", &self.records.keys().collect::<Vec<_>>())
            .field("source_files", &self.source_files)
            .field("generation_digest", &self.generation_digest)
            .finish()
    }
}

impl NodePolicySnapshot {
    pub fn require<T: TypedNodePolicy>(&self) -> anyhow::Result<&T> {
        let record = self
            .records
            .get(T::SECTION_NAME)
            .with_context(|| format!("compiled node policy omits `{}`", T::SECTION_NAME))?;
        record.as_any().downcast_ref::<T>().with_context(|| {
            format!(
                "compiled node policy `{}` has an inconsistent registered type",
                T::SECTION_NAME
            )
        })
    }

    pub fn generation_digest(&self) -> &str {
        &self.generation_digest
    }

    pub fn source_file<T: TypedNodePolicy>(&self) -> anyhow::Result<&std::path::Path> {
        self.source_files
            .get(T::SECTION_NAME)
            .map(PathBuf::as_path)
            .with_context(|| format!("compiled node policy omits `{}` source", T::SECTION_NAME))
    }

    /// Construct only the exact typed records a test exercises. Production
    /// snapshots can only be created by compiling a complete generation.
    #[cfg(feature = "test-support")]
    pub fn from_test_records(records: Vec<Arc<dyn ErasedNodePolicy>>) -> Self {
        let records = records
            .into_iter()
            .map(|record| (record.section_name(), record))
            .collect();
        Self {
            records,
            source_files: BTreeMap::new(),
            generation_digest: "test-only-node-policy-snapshot".to_owned(),
        }
    }
}

/// Compile one already validated raw generation through every registered
/// policy compiler. Missing, duplicate, or unknown sections fail closed.
pub fn compile_generation(
    app_root: &std::path::Path,
    table: &NodePolicyTable,
    generation: &generation::NodePolicyGeneration,
    signer_fingerprint: &str,
) -> anyhow::Result<NodePolicySnapshot> {
    let mut records: BTreeMap<&'static str, Arc<dyn ErasedNodePolicy>> = BTreeMap::new();
    let mut source_files = BTreeMap::new();
    for section in table.sections() {
        let name = section.name();
        let body = generation
            .policies()
            .get(name)
            .with_context(|| format!("node policy generation has no `{name}` authority"))?;
        let context = NodePolicyContext {
            section: name.to_owned(),
            source_file: generation::policy_directory(app_root).join(format!("{name}.yaml")),
            signer_fingerprint: signer_fingerprint.to_owned(),
        };
        let record = section
            .parse(&context, body)
            .with_context(|| format!("compile `{name}` node policy"))?;
        if record.section_name() != name {
            bail!(
                "node-policy compiler `{name}` returned a value for `{}`",
                record.section_name()
            );
        }
        if records.insert(name, record).is_some() {
            bail!("duplicate compiled node policy `{name}`");
        }
        source_files.insert(name, context.source_file);
    }
    if records.len() != generation.policies().len() {
        bail!("node policy generation contains an unregistered policy");
    }

    Ok(NodePolicySnapshot {
        records,
        source_files,
        generation_digest: generation.digest().to_owned(),
    })
}

/// Load and compile the exact live node-signed policy generation.
pub fn load_snapshot(
    app_root: &std::path::Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    table: &NodePolicyTable,
) -> anyhow::Result<NodePolicySnapshot> {
    let generation = generation::load_policy_generation(app_root, trust_store, table)?;
    let fingerprint = crate::identity::NodeIdentity::load(
        &ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).node_signing_key_path(),
    )?
    .fingerprint()
    .to_owned();
    compile_generation(app_root, table, &generation, &fingerprint)
}
