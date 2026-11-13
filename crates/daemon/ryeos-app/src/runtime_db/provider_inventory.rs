//! What the node's journals say it created at a provider, read for a
//! provider inventory join. Read only; bounded per source.

use super::*;
use ryeos_external_execution_contract::provider_inventory::ProviderResourceKind;

/// Rows read from any one journal. A node with more is reported as such
/// rather than joined against a partial record.
pub(crate) const MAX_NODE_PROVIDER_RECORDS: usize = 4096;

/// One provider resource the node's journals name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeProviderRecord {
    pub(crate) kind: ProviderResourceKind,
    pub(crate) provider_id: String,
    /// The journal that names it: `allocation`, `bootstrap_source`,
    /// `qualification_occurrence` or `snapshot`.
    pub(crate) source: &'static str,
    /// The node's own operation coordinate (placement thread or operation id).
    pub(crate) node_reference: String,
    /// Whether the node's record still holds the resource open. A terminal
    /// record whose resource the provider still lists is a leak.
    pub(crate) live: bool,
    /// Lifecycle adapter (backend) the record was made through.
    pub(crate) backend: String,
}

impl RuntimeDb {
    pub(crate) fn provider_resource_records(&self) -> Result<Vec<NodeProviderRecord>> {
        let mut records = self.conn.with_sql(|conn| {
            let mut records = Vec::new();
            let mut collect = |sql: &str,
                               kind: ProviderResourceKind,
                               source: &'static str,
                               id_of: &dyn Fn(&str) -> Result<String>,
                               backend_of: &dyn Fn(&str) -> Result<String>|
             -> Result<()> {
                let mut statement = conn.prepare(sql)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                anyhow::ensure!(
                    rows.len() <= MAX_NODE_PROVIDER_RECORDS,
                    "node {source} journal exceeds the inventory join bound"
                );
                for (reference, raw, live, scope) in rows {
                    records.push(NodeProviderRecord {
                        kind,
                        provider_id: id_of(&raw)?,
                        source,
                        node_reference: reference,
                        live: live != 0,
                        backend: backend_of(&scope)?,
                    });
                }
                Ok(())
            };
            let field = |name: &'static str| {
                move |raw: &str| -> Result<String> {
                    let value: serde_json::Value = serde_json::from_str(raw)?;
                    value[name]
                        .as_str()
                        .map(str::to_owned)
                        .with_context(|| format!("journal record has no `{name}`"))
                }
            };
            let verbatim = |raw: &str| -> Result<String> { Ok(raw.to_owned()) };
            // An allocation's backend comes from its retained binding,
            // resolved below; its scope column carries the binding hash.
            collect(
                "SELECT placement_thread_id, occurrence_json, phase!='terminated', reservation_json
                 FROM external_execution_allocation WHERE occurrence_json IS NOT NULL
                 ORDER BY placement_thread_id",
                ProviderResourceKind::Sandbox,
                "allocation",
                &field("occurrence_id"),
                &field("binding_hash"),
            )?;
            collect(
                "SELECT b.operation_id, b.occurrence_id, NOT EXISTS (
                     SELECT 1 FROM runtime_snapshot_bootstrap_termination t
                     WHERE t.bootstrap_operation_id=b.operation_id AND t.phase='terminal')
                 , b.provider_id
                 FROM runtime_snapshot_bootstrap b WHERE b.occurrence_id IS NOT NULL
                 ORDER BY b.operation_id",
                ProviderResourceKind::Sandbox,
                "bootstrap_source",
                &verbatim,
                &verbatim,
            )?;
            collect(
                "SELECT q.operation_id, q.occurrence_json, NOT EXISTS (
                     SELECT 1 FROM runtime_snapshot_qualification_termination t
                     WHERE t.qualification_operation_id=q.operation_id AND t.phase='terminal')
                 , q.intent_json
                 FROM runtime_snapshot_qualification q WHERE q.occurrence_json IS NOT NULL
                 ORDER BY q.operation_id",
                ProviderResourceKind::Sandbox,
                "qualification_occurrence",
                &field("occurrence_id"),
                &field("provider_id"),
            )?;
            // The node never deletes a snapshot it produced: every bound one
            // is live.
            collect(
                "SELECT operation_id, locator_json, 1, intent_json
                 FROM runtime_snapshot_operation WHERE locator_json IS NOT NULL
                 ORDER BY operation_id",
                ProviderResourceKind::Snapshot,
                "snapshot",
                &field("snapshot_id"),
                &field("provider_id"),
            )?;
            Ok(records)
        })?;
        for record in records
            .iter_mut()
            .filter(|record| record.source == "allocation")
        {
            let binding = self
                .retained_external_binding(&record.backend)?
                .context("allocation journal names no retained binding")?;
            record.backend = binding.backend_contract().backend;
        }
        Ok(records)
    }
}
