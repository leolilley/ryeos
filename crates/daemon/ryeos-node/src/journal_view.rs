//! One read over the node's signed histories: the node journal, the
//! offline-operation ledger and the trust key-event chain.
//!
//! Nothing is copied. Each row is a projection of one record that already
//! exists and is verified as it is read, and it names the source that vouches
//! for it, so a reader never sees the three as one uniform log. A record that
//! does not verify refuses the whole read; no row is skipped.
//!
//! Rows are newest first. A row's time is either inside the signed body
//! (`at_basis = body`: the journal's and the ledger's `recorded_at`) or the
//! time this node wrote in the record's signature line, which the signature
//! does not cover (`at_basis = recorded_by_node`: trust events).
//!
//! `actor` is a key fingerprint. Its basis says what stands behind it: an
//! operator's signature over the request (`operator_signature`), the
//! daemon's statement of who authenticated (`daemon_attestation`), or the
//! operator a node-signed trust event names (`recorded_by_node`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use ryeos_node_runtime::identity::NodeIdentity;
use serde::Serialize;
use serde_json::{Value, json};

use crate::node_journal::NodeJournal;
use crate::offline_operation::contract::SignedReceipt;

/// Rows a page holds when the caller names no limit.
pub const DEFAULT_PAGE_ROWS: usize = 50;
/// Most rows one page holds.
pub const MAX_PAGE_ROWS: usize = 200;
/// Most type names one query may filter on.
pub const MAX_TYPE_FILTERS: usize = 32;

/// Which signed history a row comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    NodeJournal,
    OfflineLedger,
    TrustChain,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NodeJournal => "node_journal",
            Self::OfflineLedger => "offline_ledger",
            Self::TrustChain => "trust_chain",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "node_journal" => Some(Self::NodeJournal),
            "offline_ledger" => Some(Self::OfflineLedger),
            "trust_chain" => Some(Self::TrustChain),
            _ => None,
        }
    }
}

/// Where a row's time comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AtBasis {
    /// Inside the signed body.
    Body,
    /// Written by this node in the signature line; not covered by the
    /// signature.
    RecordedByNode,
}

/// What stands behind a row's `actor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorBasis {
    OperatorSignature,
    DaemonAttestation,
    RecordedByNode,
}

/// One row of the read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JournalRow {
    /// `<source>:<sequence>:<record digest>`; names the record a reader opens.
    pub id: String,
    pub source: Source,
    #[serde(rename = "type")]
    pub row_type: String,
    pub at: String,
    pub at_basis: AtBasis,
    pub subject_ref: Option<String>,
    pub before_ref: Option<String>,
    pub after_ref: Option<String>,
    pub actor: Option<String>,
    pub actor_basis: Option<ActorBasis>,
    pub attested_by: Option<String>,
    pub detail: BTreeMap<String, Value>,
    #[serde(skip)]
    unix_secs: u64,
    #[serde(skip)]
    sequence: u64,
}

impl JournalRow {
    /// Order key, newest first: time, then source, then sequence, each
    /// descending, so equal times keep a stable order.
    fn key(&self) -> (u64, Source, u64) {
        (self.unix_secs, self.source, self.sequence)
    }
}

/// Where the held history of one source starts, when rotation dropped older
/// records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Truncation {
    pub source: Source,
    pub truncated_before: String,
    pub dropped: u64,
}

/// A query. Every filter is optional; an absent one admits every row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalQuery {
    pub types: Option<BTreeSet<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub subject: Option<String>,
    /// The `next` of a previous page: rows strictly older than it.
    pub after: Option<String>,
    pub limit: Option<usize>,
}

/// One page of rows, newest first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JournalPage {
    pub rows: Vec<JournalRow>,
    /// Cursor for the next older page; absent when this page reached the end.
    pub next: Option<String>,
    pub truncations: Vec<Truncation>,
}

fn unix_secs(field: &str, value: &str) -> Result<u64> {
    lillux::time::unix_secs_from_iso8601(value)
        .with_context(|| format!("journal row {field} {value:?} is not a canonical UTC instant"))
}

fn encode_cursor(key: (u64, Source, u64)) -> String {
    format!("{}.{}.{}", key.0, key.1.as_str(), key.2)
}

fn decode_cursor(cursor: &str) -> Result<(u64, Source, u64)> {
    let mut parts = cursor.split('.');
    let (Some(at), Some(source), Some(sequence), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("journal cursor {cursor:?} is not a cursor this read returned");
    };
    let at = at
        .parse::<u64>()
        .with_context(|| format!("journal cursor {cursor:?} is not a cursor this read returned"))?;
    let source = Source::parse(source)
        .with_context(|| format!("journal cursor {cursor:?} names no source"))?;
    let sequence = sequence
        .parse::<u64>()
        .with_context(|| format!("journal cursor {cursor:?} is not a cursor this read returned"))?;
    Ok((at, source, sequence))
}

fn digest_ref(source: Source, sequence: u64, digest: &str) -> String {
    format!("{}:{sequence}:{digest}", source.as_str())
}

/// The journal's records as rows.
pub fn journal_rows(journal: &NodeJournal) -> Result<(Vec<JournalRow>, Option<Truncation>)> {
    let read = journal.read().context("verify the node journal")?;
    let truncation = read.truncated_before.map(|truncated_before| Truncation {
        source: Source::NodeJournal,
        truncated_before,
        dropped: read.dropped_records,
    });
    let rows = read
        .records
        .into_iter()
        .map(|record| {
            let digest = record.digest()?;
            let body = record.body;
            Ok(JournalRow {
                id: digest_ref(Source::NodeJournal, body.sequence, &digest),
                source: Source::NodeJournal,
                row_type: body.record_type.as_str().to_owned(),
                unix_secs: unix_secs("recorded_at", &body.recorded_at)?,
                at: body.recorded_at,
                at_basis: AtBasis::Body,
                subject_ref: body.subject_ref,
                before_ref: body.before_ref,
                after_ref: body.after_ref,
                actor_basis: body.actor.as_ref().map(|_| ActorBasis::DaemonAttestation),
                actor: body.actor,
                attested_by: Some(body.attested_by.unwrap_or_else(|| "node".to_owned())),
                detail: body.detail,
                sequence: body.sequence,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((rows, truncation))
}

/// A digest a receipt's `result` names under `field`, when it names one.
fn result_ref(result: &Value, field: &str) -> Option<String> {
    result.get(field).and_then(Value::as_str).map(str::to_owned)
}

/// The offline ledger's receipts as rows. The row's type is the operation
/// name; the actor is the request's signer, whose signature the receipt
/// carries.
pub fn receipt_rows(
    receipts: Vec<SignedReceipt>,
    truncation: Option<Truncation>,
) -> Result<(Vec<JournalRow>, Option<Truncation>)> {
    let rows = receipts
        .into_iter()
        .map(|receipt| {
            let digest = receipt.digest()?;
            let body = receipt.body;
            let request = body.request.body;
            Ok(JournalRow {
                id: digest_ref(Source::OfflineLedger, body.sequence, &digest),
                source: Source::OfflineLedger,
                row_type: request.operation.clone(),
                unix_secs: unix_secs("recorded_at", &body.recorded_at)?,
                at: body.recorded_at,
                at_basis: AtBasis::Body,
                subject_ref: Some(body.request_digest),
                before_ref: result_ref(&body.result, "before_ref"),
                after_ref: result_ref(&body.result, "after_ref"),
                actor: Some(request.signer_fingerprint),
                actor_basis: Some(ActorBasis::OperatorSignature),
                attested_by: Some("node".to_owned()),
                detail: BTreeMap::from([("operation".to_owned(), json!(request.operation))]),
                sequence: body.sequence,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((rows, truncation))
}

/// The trust chain's events as rows, one per event, typed `trust.<kind>`.
/// The time is the event's signature line; the subject is the signer key the
/// event is about.
pub fn trust_rows(view: &crate::trust_registry::RegistryView) -> Result<Vec<JournalRow>> {
    ensure!(
        view.chain.len() == view.signed_at.len(),
        "trust chain and its signing times differ in length"
    );
    view.chain
        .iter()
        .zip(view.signed_at.iter())
        .map(|(event, signed_at)| {
            let mut detail =
                BTreeMap::from([("request_digest".to_owned(), json!(event.request_digest))]);
            if let Some(predecessor) = &event.predecessor {
                detail.insert("predecessor".to_owned(), json!(predecessor));
            }
            Ok(JournalRow {
                id: digest_ref(Source::TrustChain, event.seq, &event.digest()),
                source: Source::TrustChain,
                row_type: format!("trust.{}", event.kind.as_str()),
                unix_secs: unix_secs("signed_at", signed_at)?,
                at: signed_at.clone(),
                at_basis: AtBasis::RecordedByNode,
                subject_ref: Some(event.signer.clone()),
                before_ref: None,
                after_ref: Some(event.record_digest.clone()),
                actor: Some(event.operator.clone()),
                actor_basis: Some(ActorBasis::RecordedByNode),
                attested_by: Some("node".to_owned()),
                detail,
                sequence: event.seq,
            })
        })
        .collect()
}

/// Filter, order and page the rows of every source.
pub fn page(
    mut rows: Vec<JournalRow>,
    truncations: Vec<Truncation>,
    query: &JournalQuery,
) -> Result<JournalPage> {
    let limit = query.limit.unwrap_or(DEFAULT_PAGE_ROWS);
    ensure!(
        (1..=MAX_PAGE_ROWS).contains(&limit),
        "journal limit must be 1 to {MAX_PAGE_ROWS}"
    );
    if let Some(types) = &query.types {
        ensure!(
            !types.is_empty() && types.len() <= MAX_TYPE_FILTERS,
            "journal types must name 1 to {MAX_TYPE_FILTERS} types"
        );
    }
    let since = query
        .since
        .as_deref()
        .map(|value| unix_secs("since", value))
        .transpose()?;
    let until = query
        .until
        .as_deref()
        .map(|value| unix_secs("until", value))
        .transpose()?;
    let after = query.after.as_deref().map(decode_cursor).transpose()?;
    rows.retain(|row| {
        query
            .types
            .as_ref()
            .is_none_or(|types| types.contains(&row.row_type))
            && since.is_none_or(|since| row.unix_secs >= since)
            && until.is_none_or(|until| row.unix_secs <= until)
            && query
                .subject
                .as_ref()
                .is_none_or(|subject| row.subject_ref.as_ref() == Some(subject))
            && after.is_none_or(|after| row.key() < after)
    });
    rows.sort_by_key(|row| std::cmp::Reverse(row.key()));
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = if more {
        rows.last().map(|row| encode_cursor(row.key()))
    } else {
        None
    };
    Ok(JournalPage {
        rows,
        next,
        truncations,
    })
}

/// Read every source of `app_root`'s node and page the result.
pub fn read(app_root: &Path, node: &NodeIdentity, query: &JournalQuery) -> Result<JournalPage> {
    let mut rows = Vec::new();
    let mut truncations = Vec::new();

    let (journal, truncation) = journal_rows(&NodeJournal::for_app_root(app_root, node.clone()))?;
    rows.extend(journal);
    truncations.extend(truncation);

    let ledger = crate::offline_operation::read_receipts(app_root, node)?;
    let ledger_truncation = ledger.truncated_before.map(|truncated_before| Truncation {
        source: Source::OfflineLedger,
        truncated_before,
        dropped: ledger.dropped,
    });
    let (receipts, truncation) = receipt_rows(ledger.receipts, ledger_truncation)?;
    rows.extend(receipts);
    truncations.extend(truncation);

    let view =
        crate::trust_registry::load_view(app_root).context("read the node signer registry")?;
    rows.extend(trust_rows(&view)?);

    page(rows, truncations, query)
}

#[cfg(test)]
mod tests;
