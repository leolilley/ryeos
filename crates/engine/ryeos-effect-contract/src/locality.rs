//! Machine locality: what nodes can truthfully say about what they share.
//!
//! Each node serves graded facts about itself (a digest per relation, or an
//! "unknown" with a reason key). The node an operator is attached to collects
//! them, and [`compare_locality`] turns them, with the operator's own machine
//! declarations, into groups. The comparison is pure: it reads no clock, file
//! or socket.
//!
//! Every `as_of` is RFC 3339 in the millisecond `Z` form that
//! [`crate::rfc3339::now_millis`] and `format_unix_millis` produce, for example
//! `2026-11-18T02:30:00.123Z`.
//!
//! Rules this module holds:
//!
//! - Evidence only. Nothing here is admission, a grant or a chain identity.
//! - A relation holds between two nodes only when both report the same digest
//!   and both measured it. Nobody witnessed anything: each node speaks for
//!   itself, so no grade here is stronger than `measured`.
//! - `account` and `storage` mean something only inside one boot. Their
//!   digests are built from the raw boot id by the node, and the comparison
//!   also refuses to let them hold across boots.
//! - A node that did not answer is not a node that could not tell. They are
//!   different rows and different states.
//! - A declared machine label over nodes whose boots differ is a
//!   contradiction. It is shown, never hidden.
//! - Reasons and grades are stable keys, never sentences. Relation names are
//!   open strings: an unknown one is carried and compared, not refused.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Schema of one node's own locality facts.
pub const LOCALITY_FACTS_SCHEMA: &str = "ryeos.node.locality/v1";
/// Schema of the compared groups.
pub const LOCALITY_GROUPS_SCHEMA: &str = "ryeos.node.locality.groups/v1";

/// One kernel instance: shared fate.
pub const RELATION_BOOT: &str = "boot";
/// One backing filesystem: shared loss.
pub const RELATION_STORAGE: &str = "storage";
/// One account: shared exposure.
pub const RELATION_ACCOUNT: &str = "account";

/// Relations whose digests are scoped to a boot by construction.
const BOOT_SCOPED_RELATIONS: [&str; 2] = [RELATION_STORAGE, RELATION_ACCOUNT];

/// The kernel is emulated (a gVisor sandbox), so its boot id says nothing.
pub const REASON_EMULATED_KERNEL: &str = "emulated_kernel";
/// This host or this process cannot measure the value.
pub const REASON_NOT_MEASURABLE: &str = "not_measurable";
/// The process runs in a nested user namespace, where a uid names nothing
/// outside it.
pub const REASON_USER_NAMESPACE: &str = "user_namespace";
/// The node answered but reported nothing for this relation.
pub const REASON_NOT_REPORTED: &str = "not_reported";

/// The remote did not answer in time.
pub const REASON_TIMEOUT: &str = "timeout";
/// The remote could not be reached.
pub const REASON_UNREACHABLE: &str = "unreachable";
/// The remote does not serve `service:node/locality`.
pub const REASON_SERVICE_NOT_INSTALLED: &str = "service_not_installed";
/// The remote is inbound-only: this node never dials it.
pub const REASON_INBOUND_ONLY: &str = "inbound_only";
/// The remote's pin failed verification, so there is nothing to dial.
pub const REASON_REMOTE_INVALID: &str = "remote_invalid";
/// The remote answered with an error that is not a refusal.
pub const REASON_REMOTE_ERROR: &str = "remote_error";
/// The remote answered with something that is not valid locality facts.
pub const REASON_INVALID_ANSWER: &str = "invalid_answer";

/// The grade of one relation fact, with the meaning of the same names in
/// `lillux::process_control::ProofSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalityGrade {
    /// Observed by the node on its own host.
    Measured,
    /// Not observed, or observed in a way that proves nothing.
    Unknown,
}

impl LocalityGrade {
    /// The proof source with the same meaning.
    pub fn proof_source(self) -> lillux::process_control::ProofSource {
        match self {
            Self::Measured => lillux::process_control::ProofSource::Measured,
            Self::Unknown => lillux::process_control::ProofSource::Unknown("unknown".to_owned()),
        }
    }
}

/// What one node says about one relation: a digest it measured, or that it
/// could not tell and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "grade", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelationFact {
    Measured { digest: String },
    Unknown { reason: String },
}

impl RelationFact {
    pub fn grade(&self) -> LocalityGrade {
        match self {
            Self::Measured { .. } => LocalityGrade::Measured,
            Self::Unknown { .. } => LocalityGrade::Unknown,
        }
    }

    pub fn digest(&self) -> Option<&str> {
        match self {
            Self::Measured { digest } => Some(digest),
            Self::Unknown { .. } => None,
        }
    }

    fn validate(&self, relation: &str) -> Result<(), LocalityError> {
        match self {
            Self::Measured { digest } => {
                if !is_digest(digest) {
                    return Err(LocalityError::Invalid(format!(
                        "relation `{relation}` digest is not 64 lowercase hex characters"
                    )));
                }
            }
            Self::Unknown { reason } => require_key("reason", reason)?,
        }
        Ok(())
    }
}

/// One node's own locality facts, as `service:node/locality` returns them.
/// Digests only: never a boot id, path, mount, device or uid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeLocalityFacts {
    pub schema: String,
    pub relations: BTreeMap<String, RelationFact>,
}

impl NodeLocalityFacts {
    pub fn new(relations: BTreeMap<String, RelationFact>) -> Self {
        Self {
            schema: LOCALITY_FACTS_SCHEMA.to_owned(),
            relations,
        }
    }

    pub fn validate(&self) -> Result<(), LocalityError> {
        if self.schema != LOCALITY_FACTS_SCHEMA {
            return Err(LocalityError::Invalid(format!(
                "locality facts schema `{}` is not `{LOCALITY_FACTS_SCHEMA}`",
                self.schema
            )));
        }
        for (relation, fact) in &self.relations {
            require_key("relation", relation)?;
            fact.validate(relation)?;
        }
        Ok(())
    }
}

/// What the fan-out learned from one node. "Did not answer" is not "unknown":
/// the first says nothing was learned, the second is something a node said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeAnswer {
    Answered {
        facts: NodeLocalityFacts,
        as_of: String,
    },
    DidNotAnswer {
        reason: String,
        as_of: String,
    },
    /// The node refused: its operator has not granted the capability.
    NotGranted {
        as_of: String,
    },
}

/// How a member relates to the node doing the comparing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    /// The node doing the comparing.
    This,
    /// A configured remote.
    Remote,
    /// A node service installed on this host that this node has no remote for.
    /// It is listed, never asked.
    InstalledNotConnected,
}

/// One node as the comparison receives it. `id` is the stable id (the site
/// id; for an installed-not-connected node, its service association name).
/// `name` is what the operator calls it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInput {
    pub id: String,
    pub name: String,
    pub kind: MemberKind,
    /// `None` exactly for [`MemberKind::InstalledNotConnected`].
    pub answer: Option<NodeAnswer>,
}

/// The operator's statement that these nodes are one machine. It is declared,
/// not measured, and no node admits anything on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineDeclaration {
    pub label: String,
    /// Stable node ids (site ids).
    pub members: Vec<String>,
}

/// The named state of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupState {
    /// Named by the operator, and the members share a boot.
    DeclaredConfirmed,
    /// Named by the operator, with no evidence either way.
    Declared,
    /// Named by the operator, but members report different boots. The group's
    /// `disagreeing` lists them.
    DeclaredContradicted,
    /// Nodes share a boot and nobody named the group.
    Observed,
    /// One node, and nothing else shares its boot.
    Alone,
    /// Nothing can be told about this node's host.
    CannotTell,
}

/// What a member's row says about the fan-out. Carries no facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemberAnswer {
    Answered { as_of: String },
    DidNotAnswer { reason: String, as_of: String },
    NotGranted { as_of: String },
    NotAsked,
}

/// A member's grade for one relation. The digest is not repeated here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberFact {
    pub grade: LocalityGrade,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The ids of the OTHER members that report the same measured digest for
    /// this relation. Empty when none, and always empty for an `unknown` fact.
    /// It never disagrees with the group's measured [`RelationRow`]s.
    #[serde(default)]
    pub shared_with: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberRow {
    pub id: String,
    pub name: String,
    pub kind: MemberKind,
    /// Whether the operator's declaration names this member.
    pub declared: bool,
    /// Whether this member's id is in the group's `disagreeing`.
    #[serde(default)]
    pub disagrees: bool,
    pub answer: MemberAnswer,
    pub facts: BTreeMap<String, MemberFact>,
}

/// A relation between the members of a group. A `measured` row names members
/// that report the same digest. An `unknown` row names members that answered
/// but could not tell, with the reason key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationRow {
    pub relation: String,
    pub grade: LocalityGrade,
    pub members: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalityGroup {
    /// Stable key: `machine:<label>` for a declared group, `node:<least id>`
    /// otherwise.
    pub key: String,
    /// The declared machine label, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub state: GroupState,
    /// Whether the group holds the member of kind `this`: the node doing the
    /// comparing.
    #[serde(default)]
    pub this_computer: bool,
    /// Members whose boot disagrees with the rest. Empty unless the state is
    /// `declared_contradicted`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disagreeing: Vec<String>,
    /// Ids a declaration names that are not among the nodes compared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declared_missing: Vec<String>,
    pub members: Vec<MemberRow>,
    pub relations: Vec<RelationRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalityGroups {
    pub schema: String,
    pub as_of: String,
    pub groups: Vec<LocalityGroup>,
}

/// Why the comparison refused its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalityError {
    Invalid(String),
    /// Two nodes carry one id.
    DuplicateNode(String),
    /// Two declarations carry one label.
    DuplicateLabel(String),
    /// One node is named by two declarations. A node is on one machine; the
    /// comparison refuses to pick which declaration is right.
    NodeInTwoDeclarations {
        id: String,
        labels: Vec<String>,
    },
}

impl fmt::Display for LocalityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid locality input: {message}"),
            Self::DuplicateNode(id) => write!(f, "node `{id}` is listed twice"),
            Self::DuplicateLabel(label) => {
                write!(f, "machine label `{label}` is declared twice")
            }
            Self::NodeInTwoDeclarations { id, labels } => write!(
                f,
                "node `{id}` is named by two machine declarations: {}",
                labels.join(", ")
            ),
        }
    }
}

impl std::error::Error for LocalityError {}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Keys are short lowercase tokens so they stay stable across languages and
/// wording changes.
fn require_key(what: &str, value: &str) -> Result<(), LocalityError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        });
    if valid {
        Ok(())
    } else {
        Err(LocalityError::Invalid(format!(
            "{what} `{value}` is not a stable key"
        )))
    }
}

fn boot_digest(node: &NodeInput) -> Option<&str> {
    match &node.answer {
        Some(NodeAnswer::Answered { facts, .. }) => facts
            .relations
            .get(RELATION_BOOT)
            .and_then(RelationFact::digest),
        _ => None,
    }
}

fn member_row(node: &NodeInput, declared: bool) -> MemberRow {
    let (answer, facts) = match &node.answer {
        Some(NodeAnswer::Answered { facts, as_of }) => (
            MemberAnswer::Answered {
                as_of: as_of.clone(),
            },
            facts
                .relations
                .iter()
                .map(|(relation, fact)| {
                    let member = match fact {
                        RelationFact::Measured { .. } => MemberFact {
                            grade: LocalityGrade::Measured,
                            reason: None,
                            shared_with: Vec::new(),
                        },
                        RelationFact::Unknown { reason } => MemberFact {
                            grade: LocalityGrade::Unknown,
                            reason: Some(reason.clone()),
                            shared_with: Vec::new(),
                        },
                    };
                    (relation.clone(), member)
                })
                .collect(),
        ),
        Some(NodeAnswer::DidNotAnswer { reason, as_of }) => (
            MemberAnswer::DidNotAnswer {
                reason: reason.clone(),
                as_of: as_of.clone(),
            },
            BTreeMap::new(),
        ),
        Some(NodeAnswer::NotGranted { as_of }) => (
            MemberAnswer::NotGranted {
                as_of: as_of.clone(),
            },
            BTreeMap::new(),
        ),
        None => (MemberAnswer::NotAsked, BTreeMap::new()),
    };
    MemberRow {
        id: node.id.clone(),
        name: node.name.clone(),
        kind: node.kind,
        declared,
        disagrees: false,
        answer,
        facts,
    }
}

/// The relations among `members`. A relation holds between members that both
/// measured the same digest; for a boot-scoped relation they must also share a
/// measured boot. Members that answered but could not tell are named by an
/// unknown row with the reason.
fn relation_rows(members: &[&NodeInput]) -> Vec<RelationRow> {
    if members.len() < 2 {
        return Vec::new();
    }
    let mut names = BTreeSet::new();
    for member in members {
        if let Some(NodeAnswer::Answered { facts, .. }) = &member.answer {
            names.extend(facts.relations.keys().cloned());
        }
    }
    let mut rows = Vec::new();
    for relation in names {
        let scoped = BOOT_SCOPED_RELATIONS.contains(&relation.as_str());
        let mut held: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        let mut unknown: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for member in members {
            let Some(NodeAnswer::Answered { facts, .. }) = &member.answer else {
                continue;
            };
            match facts.relations.get(&relation) {
                Some(RelationFact::Measured { digest }) => {
                    let scope = if scoped {
                        match boot_digest(member) {
                            Some(boot) => boot.to_owned(),
                            None => {
                                unknown
                                    .entry(REASON_NOT_MEASURABLE.to_owned())
                                    .or_default()
                                    .push(member.id.clone());
                                continue;
                            }
                        }
                    } else {
                        String::new()
                    };
                    held.entry((scope, digest.clone()))
                        .or_default()
                        .push(member.id.clone());
                }
                Some(RelationFact::Unknown { reason }) => {
                    unknown
                        .entry(reason.clone())
                        .or_default()
                        .push(member.id.clone());
                }
                None => {
                    unknown
                        .entry(REASON_NOT_REPORTED.to_owned())
                        .or_default()
                        .push(member.id.clone());
                }
            }
        }
        let mut found = Vec::new();
        for (_, mut ids) in held {
            if ids.len() >= 2 {
                ids.sort();
                found.push(RelationRow {
                    relation: relation.clone(),
                    grade: LocalityGrade::Measured,
                    members: ids,
                    reason: None,
                });
            }
        }
        for (reason, mut ids) in unknown {
            ids.sort();
            found.push(RelationRow {
                relation: relation.clone(),
                grade: LocalityGrade::Unknown,
                members: ids,
                reason: Some(reason),
            });
        }
        found.sort_by(|a, b| {
            (a.grade, a.members.first(), a.reason.as_ref()).cmp(&(
                b.grade,
                b.members.first(),
                b.reason.as_ref(),
            ))
        });
        rows.extend(found);
    }
    rows
}

/// The state of a declared group over the members present among the nodes,
/// and the ids that disagree when the boots contradict the declaration.
fn declared_state(members: &[&NodeInput]) -> (GroupState, Vec<String>) {
    let mut clusters: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for member in members {
        if let Some(digest) = boot_digest(member) {
            clusters.entry(digest).or_default().push(&member.id);
        }
    }
    if clusters.len() >= 2 {
        let largest = clusters.values().map(Vec::len).max().unwrap_or(0);
        let leaders: Vec<_> = clusters
            .values()
            .filter(|ids| ids.len() == largest)
            .collect();
        let mut disagreeing: Vec<String> = if leaders.len() == 1 {
            clusters
                .values()
                .filter(|ids| ids.len() != largest)
                .flatten()
                .map(|id| (*id).to_owned())
                .collect()
        } else {
            // No side holds a strict majority, so none is the odd one out.
            clusters
                .values()
                .flatten()
                .map(|id| (*id).to_owned())
                .collect()
        };
        disagreeing.sort();
        return (GroupState::DeclaredContradicted, disagreeing);
    }
    let evidenced: usize = clusters.values().map(Vec::len).sum();
    if members.len() >= 2 && evidenced == members.len() {
        (GroupState::DeclaredConfirmed, Vec::new())
    } else {
        (GroupState::Declared, Vec::new())
    }
}

/// One group's rows, with the shapes a view draws directly: who shares what,
/// who disagrees, and whether this node is in it.
fn assemble_group(
    key: String,
    label: Option<String>,
    state: GroupState,
    disagreeing: Vec<String>,
    declared_missing: Vec<String>,
    members: &[&NodeInput],
    declared: bool,
) -> LocalityGroup {
    let relations = relation_rows(members);
    let mut rows: Vec<MemberRow> = members
        .iter()
        .map(|node| member_row(node, declared))
        .collect();
    for row in &mut rows {
        row.disagrees = disagreeing.contains(&row.id);
        // `shared_with` is read off the measured relation rows, so the two can
        // never disagree.
        for relation in relations
            .iter()
            .filter(|relation| relation.grade == LocalityGrade::Measured)
            .filter(|relation| relation.members.contains(&row.id))
        {
            if let Some(fact) = row.facts.get_mut(&relation.relation) {
                fact.shared_with = relation
                    .members
                    .iter()
                    .filter(|id| **id != row.id)
                    .cloned()
                    .collect();
            }
        }
    }
    LocalityGroup {
        key,
        label,
        state,
        this_computer: members.iter().any(|node| node.kind == MemberKind::This),
        disagreeing,
        declared_missing,
        members: rows,
        relations,
    }
}

/// Compare the nodes' answers with the operator's declarations.
///
/// Declared groups come first, by label, each holding the declared members
/// that are among `nodes`. Every other node is grouped by measured equal boot:
/// two or more is `observed`, one with a measured boot is `alone`, and one
/// with no measured boot (unknown, did not answer, refused, or never asked) is
/// `cannot_tell`. An undeclared node never joins a declared group.
///
/// A node named by two declarations, a label declared twice and a duplicate
/// node id are refused rather than resolved.
pub fn compare_locality(
    as_of: &str,
    nodes: &[NodeInput],
    declarations: &[MachineDeclaration],
) -> Result<LocalityGroups, LocalityError> {
    let mut by_id: BTreeMap<&str, &NodeInput> = BTreeMap::new();
    for node in nodes {
        if node.id.is_empty() || node.name.is_empty() {
            return Err(LocalityError::Invalid(
                "a node needs an id and a name".to_owned(),
            ));
        }
        if by_id.insert(&node.id, node).is_some() {
            return Err(LocalityError::DuplicateNode(node.id.clone()));
        }
        match (&node.kind, &node.answer) {
            (MemberKind::InstalledNotConnected, Some(_)) => {
                return Err(LocalityError::Invalid(format!(
                    "node `{}` is not connected, so it cannot have an answer",
                    node.id
                )));
            }
            (MemberKind::This | MemberKind::Remote, None) => {
                return Err(LocalityError::Invalid(format!(
                    "node `{}` was asked, so it needs an answer",
                    node.id
                )));
            }
            (_, Some(NodeAnswer::Answered { facts, .. })) => facts.validate()?,
            (_, Some(NodeAnswer::DidNotAnswer { reason, .. })) => require_key("reason", reason)?,
            _ => {}
        }
    }

    let mut labels: BTreeMap<&str, &MachineDeclaration> = BTreeMap::new();
    let mut declared_by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for declaration in declarations {
        if declaration.label.is_empty() {
            return Err(LocalityError::Invalid(
                "a machine declaration needs a label".to_owned(),
            ));
        }
        if labels.insert(&declaration.label, declaration).is_some() {
            return Err(LocalityError::DuplicateLabel(declaration.label.clone()));
        }
        let unique: BTreeSet<&str> = declaration.members.iter().map(String::as_str).collect();
        for id in unique {
            declared_by.entry(id).or_default().push(&declaration.label);
        }
    }
    for (id, named_by) in &declared_by {
        if named_by.len() > 1 {
            let mut labels: Vec<String> =
                named_by.iter().map(|label| (*label).to_owned()).collect();
            labels.sort();
            return Err(LocalityError::NodeInTwoDeclarations {
                id: (*id).to_owned(),
                labels,
            });
        }
    }

    let mut groups = Vec::new();
    for (label, declaration) in &labels {
        let ids: BTreeSet<&str> = declaration.members.iter().map(String::as_str).collect();
        let present: Vec<&NodeInput> = ids.iter().filter_map(|id| by_id.get(id).copied()).collect();
        let declared_missing: Vec<String> = ids
            .iter()
            .filter(|id| !by_id.contains_key(*id))
            .map(|id| (*id).to_owned())
            .collect();
        let (state, disagreeing) = declared_state(&present);
        groups.push(assemble_group(
            format!("machine:{label}"),
            Some((*label).to_owned()),
            state,
            disagreeing,
            declared_missing,
            &present,
            true,
        ));
    }

    // Everything undeclared: components by measured equal boot.
    let mut by_boot: BTreeMap<&str, Vec<&NodeInput>> = BTreeMap::new();
    let mut without_boot: Vec<&NodeInput> = Vec::new();
    for node in by_id.values().copied() {
        if declared_by.contains_key(node.id.as_str()) {
            continue;
        }
        match boot_digest(node) {
            Some(digest) => by_boot.entry(digest).or_default().push(node),
            None => without_boot.push(node),
        }
    }
    let mut undeclared = Vec::new();
    for (_, members) in by_boot {
        let state = if members.len() >= 2 {
            GroupState::Observed
        } else {
            GroupState::Alone
        };
        undeclared.push((state, members));
    }
    for node in without_boot {
        undeclared.push((GroupState::CannotTell, vec![node]));
    }
    let mut undeclared: Vec<LocalityGroup> = undeclared
        .into_iter()
        .map(|(state, mut members)| {
            members.sort_by(|a, b| a.id.cmp(&b.id));
            assemble_group(
                format!("node:{}", members[0].id),
                None,
                state,
                Vec::new(),
                Vec::new(),
                &members,
                false,
            )
        })
        .collect();
    undeclared.sort_by(|a, b| a.key.cmp(&b.key));
    groups.extend(undeclared);

    Ok(LocalityGroups {
        schema: LOCALITY_GROUPS_SCHEMA.to_owned(),
        as_of: as_of.to_owned(),
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AS_OF: &str = "2026-11-18T00:00:00.000Z";

    fn digest(seed: &str) -> String {
        lillux::sha256_hex(seed.as_bytes())
    }

    fn measured(seed: &str) -> RelationFact {
        RelationFact::Measured {
            digest: digest(seed),
        }
    }

    fn unknown(reason: &str) -> RelationFact {
        RelationFact::Unknown {
            reason: reason.to_owned(),
        }
    }

    fn facts(relations: &[(&str, RelationFact)]) -> NodeLocalityFacts {
        NodeLocalityFacts::new(
            relations
                .iter()
                .map(|(name, fact)| ((*name).to_owned(), fact.clone()))
                .collect(),
        )
    }

    fn answered(id: &str, relations: &[(&str, RelationFact)]) -> NodeInput {
        NodeInput {
            id: id.to_owned(),
            name: format!("name-{id}"),
            kind: MemberKind::Remote,
            answer: Some(NodeAnswer::Answered {
                facts: facts(relations),
                as_of: AS_OF.to_owned(),
            }),
        }
    }

    fn with_answer(id: &str, answer: NodeAnswer) -> NodeInput {
        NodeInput {
            id: id.to_owned(),
            name: format!("name-{id}"),
            kind: MemberKind::Remote,
            answer: Some(answer),
        }
    }

    fn declaration(label: &str, members: &[&str]) -> MachineDeclaration {
        MachineDeclaration {
            label: label.to_owned(),
            members: members.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn compare(nodes: &[NodeInput], declarations: &[MachineDeclaration]) -> Vec<LocalityGroup> {
        compare_locality(AS_OF, nodes, declarations).unwrap().groups
    }

    fn group_of<'a>(groups: &'a [LocalityGroup], id: &str) -> &'a LocalityGroup {
        groups
            .iter()
            .find(|g| g.members.iter().any(|m| m.id == id))
            .unwrap()
    }

    fn ids(group: &LocalityGroup) -> Vec<&str> {
        group
            .members
            .iter()
            .map(|member| member.id.as_str())
            .collect()
    }

    #[test]
    fn two_nodes_on_one_boot_are_an_observed_group() {
        let groups = compare(
            &[
                answered(
                    "b",
                    &[
                        ("boot", measured("boot-1")),
                        ("account", measured("acct-1")),
                    ],
                ),
                answered(
                    "a",
                    &[
                        ("boot", measured("boot-1")),
                        ("account", measured("acct-1")),
                    ],
                ),
            ],
            &[],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, GroupState::Observed);
        assert_eq!(groups[0].key, "node:a");
        assert_eq!(groups[0].label, None);
        assert_eq!(ids(&groups[0]), ["a", "b"]);
        let relations: Vec<_> = groups[0]
            .relations
            .iter()
            .map(|row| (row.relation.as_str(), row.grade, row.members.clone()))
            .collect();
        assert_eq!(
            relations,
            [
                (
                    "account",
                    LocalityGrade::Measured,
                    vec!["a".to_owned(), "b".to_owned()]
                ),
                (
                    "boot",
                    LocalityGrade::Measured,
                    vec!["a".to_owned(), "b".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn same_boot_different_account_shares_the_boot_only() {
        let groups = compare(
            &[
                answered(
                    "a",
                    &[
                        ("boot", measured("boot-1")),
                        ("account", measured("acct-1")),
                    ],
                ),
                answered(
                    "b",
                    &[
                        ("boot", measured("boot-1")),
                        ("account", measured("acct-2")),
                    ],
                ),
            ],
            &[],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, GroupState::Observed);
        let measured_relations: Vec<_> = groups[0]
            .relations
            .iter()
            .filter(|row| row.grade == LocalityGrade::Measured)
            .map(|row| row.relation.as_str())
            .collect();
        assert_eq!(measured_relations, ["boot"]);
    }

    #[test]
    fn different_boots_are_separate_alone_groups() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                answered("b", &[("boot", measured("boot-2"))]),
            ],
            &[],
        );
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|group| group.state == GroupState::Alone));
        assert!(groups.iter().all(|group| group.relations.is_empty()));
    }

    #[test]
    fn a_scoped_relation_never_holds_across_boots() {
        // Two nodes on different boots that report one account digest are
        // inconsistent; the account still does not hold, and they are not
        // grouped by it.
        let groups = compare(
            &[
                answered(
                    "a",
                    &[
                        ("boot", measured("boot-1")),
                        ("account", measured("uid-1000")),
                    ],
                ),
                answered(
                    "b",
                    &[
                        ("boot", measured("boot-2")),
                        ("account", measured("uid-1000")),
                    ],
                ),
            ],
            &[declaration("workstation", &["a", "b"])],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, GroupState::DeclaredContradicted);
        assert!(
            groups[0]
                .relations
                .iter()
                .all(|row| row.grade != LocalityGrade::Measured)
        );
    }

    #[test]
    fn a_declared_group_whose_members_share_a_boot_is_confirmed() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                answered("b", &[("boot", measured("boot-1"))]),
            ],
            &[declaration("workstation", &["a", "b"])],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, GroupState::DeclaredConfirmed);
        assert_eq!(groups[0].key, "machine:workstation");
        assert_eq!(groups[0].label.as_deref(), Some("workstation"));
        assert!(groups[0].members.iter().all(|member| member.declared));
        assert!(groups[0].disagreeing.is_empty());
    }

    #[test]
    fn a_declared_group_over_different_boots_is_contradicted_and_names_who_disagrees() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                answered("b", &[("boot", measured("boot-1"))]),
                answered("c", &[("boot", measured("boot-2"))]),
            ],
            &[declaration("workstation", &["a", "b", "c"])],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, GroupState::DeclaredContradicted);
        assert_eq!(groups[0].disagreeing, ["c"]);
        // The contradiction is shown with every member, not dropped.
        assert_eq!(ids(&groups[0]), ["a", "b", "c"]);
    }

    #[test]
    fn an_even_split_has_no_odd_one_out() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                answered("b", &[("boot", measured("boot-2"))]),
            ],
            &[declaration("workstation", &["a", "b"])],
        );
        assert_eq!(groups[0].state, GroupState::DeclaredContradicted);
        assert_eq!(groups[0].disagreeing, ["a", "b"]);
    }

    #[test]
    fn a_declared_group_with_no_evidence_is_only_declared() {
        let groups = compare(
            &[
                answered("a", &[("boot", unknown(REASON_EMULATED_KERNEL))]),
                with_answer(
                    "b",
                    NodeAnswer::DidNotAnswer {
                        reason: REASON_TIMEOUT.to_owned(),
                        as_of: AS_OF.to_owned(),
                    },
                ),
            ],
            &[declaration("workstation", &["a", "b"])],
        );
        assert_eq!(groups[0].state, GroupState::Declared);
    }

    #[test]
    fn a_declared_group_with_partial_evidence_is_not_confirmed() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                answered("b", &[("boot", measured("boot-1"))]),
                answered("c", &[("boot", unknown(REASON_NOT_MEASURABLE))]),
            ],
            &[declaration("workstation", &["a", "b", "c"])],
        );
        assert_eq!(groups[0].state, GroupState::Declared);
        // What is known stays visible as a relation.
        assert!(groups[0].relations.iter().any(|row| {
            row.relation == "boot"
                && row.grade == LocalityGrade::Measured
                && row.members == ["a", "b"]
        }));
        assert!(groups[0].relations.iter().any(|row| {
            row.relation == "boot"
                && row.grade == LocalityGrade::Unknown
                && row.members == ["c"]
                && row.reason.as_deref() == Some(REASON_NOT_MEASURABLE)
        }));
    }

    #[test]
    fn a_declared_id_that_is_not_a_node_is_reported_not_invented() {
        let groups = compare(
            &[answered("a", &[("boot", measured("boot-1"))])],
            &[declaration("workstation", &["a", "ghost"])],
        );
        assert_eq!(groups[0].state, GroupState::Declared);
        assert_eq!(ids(&groups[0]), ["a"]);
        assert_eq!(groups[0].declared_missing, ["ghost"]);
    }

    #[test]
    fn an_emulated_kernel_cannot_tell_and_is_not_grouped_with_a_native_node() {
        let groups = compare(
            &[
                answered("native", &[("boot", measured("boot-1"))]),
                answered(
                    "sandbox",
                    &[
                        ("boot", unknown(REASON_EMULATED_KERNEL)),
                        ("account", unknown(REASON_EMULATED_KERNEL)),
                    ],
                ),
            ],
            &[],
        );
        assert_eq!(groups.len(), 2);
        let sandbox = groups.iter().find(|g| ids(g) == ["sandbox"]).unwrap();
        assert_eq!(sandbox.state, GroupState::CannotTell);
        assert_eq!(
            sandbox.members[0].facts["boot"],
            MemberFact {
                grade: LocalityGrade::Unknown,
                reason: Some(REASON_EMULATED_KERNEL.to_owned()),
                shared_with: Vec::new(),
            }
        );
        let native = groups.iter().find(|g| ids(g) == ["native"]).unwrap();
        assert_eq!(native.state, GroupState::Alone);
    }

    #[test]
    fn two_unknown_boots_never_match() {
        let groups = compare(
            &[
                answered("a", &[("boot", unknown(REASON_EMULATED_KERNEL))]),
                answered("b", &[("boot", unknown(REASON_EMULATED_KERNEL))]),
            ],
            &[],
        );
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.state == GroupState::CannotTell));
    }

    #[test]
    fn a_node_that_did_not_answer_is_not_the_same_as_one_that_cannot_tell() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                with_answer(
                    "stopped",
                    NodeAnswer::DidNotAnswer {
                        reason: REASON_UNREACHABLE.to_owned(),
                        as_of: AS_OF.to_owned(),
                    },
                ),
                answered("sandbox", &[("boot", unknown(REASON_EMULATED_KERNEL))]),
            ],
            &[],
        );
        let stopped = groups.iter().find(|g| ids(g) == ["stopped"]).unwrap();
        assert_eq!(stopped.state, GroupState::CannotTell);
        assert_eq!(
            stopped.members[0].answer,
            MemberAnswer::DidNotAnswer {
                reason: REASON_UNREACHABLE.to_owned(),
                as_of: AS_OF.to_owned(),
            }
        );
        assert!(stopped.members[0].facts.is_empty());
        let sandbox = groups.iter().find(|g| ids(g) == ["sandbox"]).unwrap();
        assert_eq!(
            sandbox.members[0].answer,
            MemberAnswer::Answered {
                as_of: AS_OF.to_owned()
            }
        );
    }

    #[test]
    fn a_refusal_is_its_own_row_state() {
        let groups = compare(
            &[with_answer(
                "locked",
                NodeAnswer::NotGranted {
                    as_of: AS_OF.to_owned(),
                },
            )],
            &[],
        );
        assert_eq!(groups[0].state, GroupState::CannotTell);
        assert_eq!(
            groups[0].members[0].answer,
            MemberAnswer::NotGranted {
                as_of: AS_OF.to_owned()
            }
        );
    }

    #[test]
    fn an_installed_node_is_listed_not_asked_and_makes_no_claim() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                NodeInput {
                    id: "ryeos-0123".to_owned(),
                    name: "ryeos-0123".to_owned(),
                    kind: MemberKind::InstalledNotConnected,
                    answer: None,
                },
            ],
            &[],
        );
        assert_eq!(groups.len(), 2);
        let installed = groups.iter().find(|g| ids(g) == ["ryeos-0123"]).unwrap();
        assert_eq!(installed.state, GroupState::CannotTell);
        assert_eq!(installed.members[0].answer, MemberAnswer::NotAsked);
        assert_eq!(installed.members[0].kind, MemberKind::InstalledNotConnected);
    }

    #[test]
    fn a_node_in_two_declarations_is_refused() {
        let error = compare_locality(
            AS_OF,
            &[answered("a", &[("boot", measured("boot-1"))])],
            &[declaration("one", &["a"]), declaration("two", &["a"])],
        )
        .unwrap_err();
        assert_eq!(
            error,
            LocalityError::NodeInTwoDeclarations {
                id: "a".to_owned(),
                labels: vec!["one".to_owned(), "two".to_owned()],
            }
        );
    }

    #[test]
    fn a_repeated_id_inside_one_declaration_is_one_member() {
        let groups = compare(
            &[answered("a", &[("boot", measured("boot-1"))])],
            &[declaration("one", &["a", "a"])],
        );
        assert_eq!(ids(&groups[0]), ["a"]);
    }

    #[test]
    fn duplicate_labels_and_duplicate_nodes_are_refused() {
        let node = answered("a", &[("boot", measured("boot-1"))]);
        assert_eq!(
            compare_locality(
                AS_OF,
                std::slice::from_ref(&node),
                &[declaration("one", &["a"]), declaration("one", &[])]
            )
            .unwrap_err(),
            LocalityError::DuplicateLabel("one".to_owned())
        );
        assert_eq!(
            compare_locality(AS_OF, &[node.clone(), node], &[]).unwrap_err(),
            LocalityError::DuplicateNode("a".to_owned())
        );
    }

    #[test]
    fn an_undeclared_node_does_not_join_a_declared_group() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                answered("b", &[("boot", measured("boot-1"))]),
                answered("c", &[("boot", measured("boot-1"))]),
            ],
            &[declaration("workstation", &["a", "b"])],
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].state, GroupState::DeclaredConfirmed);
        assert_eq!(ids(&groups[0]), ["a", "b"]);
        assert_eq!(groups[1].state, GroupState::Alone);
        assert_eq!(ids(&groups[1]), ["c"]);
    }

    #[test]
    fn an_unknown_relation_name_is_carried_and_compared() {
        let groups = compare(
            &[
                answered(
                    "a",
                    &[("boot", measured("boot-1")), ("rack", measured("r1"))],
                ),
                answered(
                    "b",
                    &[("boot", measured("boot-1")), ("rack", measured("r1"))],
                ),
            ],
            &[],
        );
        assert!(groups[0].relations.iter().any(|row| row.relation == "rack"
            && row.grade == LocalityGrade::Measured
            && row.members == ["a", "b"]));
    }

    #[test]
    fn an_answered_node_that_omits_a_relation_is_named_not_reported() {
        let groups = compare(
            &[
                answered(
                    "a",
                    &[("boot", measured("boot-1")), ("storage", measured("s"))],
                ),
                answered("b", &[("boot", measured("boot-1"))]),
            ],
            &[],
        );
        assert!(
            groups[0]
                .relations
                .iter()
                .any(|row| row.relation == "storage"
                    && row.grade == LocalityGrade::Unknown
                    && row.reason.as_deref() == Some(REASON_NOT_REPORTED)
                    && row.members == ["b"])
        );
    }

    #[test]
    fn invalid_facts_are_refused() {
        let mut bad = answered("a", &[]);
        if let Some(NodeAnswer::Answered { facts, .. }) = bad.answer.as_mut() {
            facts.relations.insert(
                "boot".to_owned(),
                RelationFact::Measured {
                    digest: "not-a-digest".to_owned(),
                },
            );
        }
        assert!(matches!(
            compare_locality(AS_OF, &[bad], &[]),
            Err(LocalityError::Invalid(_))
        ));
        let sentence = answered("a", &[("boot", unknown("The kernel is emulated"))]);
        assert!(matches!(
            compare_locality(AS_OF, &[sentence], &[]),
            Err(LocalityError::Invalid(_))
        ));
    }

    #[test]
    fn contract_types_reject_unknown_fields_and_round_trip() {
        let node_facts = facts(&[
            ("boot", measured("boot-1")),
            ("storage", unknown("not_measurable")),
        ]);
        let value = serde_json::to_value(&node_facts).unwrap();
        assert_eq!(value["schema"], LOCALITY_FACTS_SCHEMA);
        assert_eq!(value["relations"]["boot"]["grade"], "measured");
        assert_eq!(value["relations"]["storage"]["reason"], "not_measurable");
        let back: NodeLocalityFacts = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, node_facts);

        let mut extra = value;
        extra["boot_id"] = serde_json::json!("00000000-0000-0000-0000-000000000000");
        assert!(serde_json::from_value::<NodeLocalityFacts>(extra).is_err());
        assert!(
            serde_json::from_value::<RelationFact>(serde_json::json!({
                "grade": "measured", "digest": digest("x"), "path": "/x"
            }))
            .is_err()
        );

        let groups = compare_locality(
            AS_OF,
            &[answered("a", &[("boot", measured("boot-1"))])],
            &[],
        )
        .unwrap();
        let value = serde_json::to_value(&groups).unwrap();
        assert_eq!(value["schema"], LOCALITY_GROUPS_SCHEMA);
        assert_eq!(value["groups"][0]["state"], "alone");
        assert_eq!(
            value["groups"][0]["members"][0]["answer"]["state"],
            "answered"
        );
        let back: LocalityGroups = serde_json::from_value(value).unwrap();
        assert_eq!(back, groups);
    }

    #[test]
    fn rows_say_who_shares_who_disagrees_and_which_group_is_this_computer() {
        let mut this = answered(
            "a",
            &[("boot", measured("boot-1")), ("account", measured("uid"))],
        );
        this.kind = MemberKind::This;
        let groups = compare(
            &[
                this,
                answered(
                    "b",
                    &[("boot", measured("boot-1")), ("account", measured("other"))],
                ),
                answered(
                    "c",
                    &[("boot", measured("boot-1")), ("account", measured("uid"))],
                ),
                answered("d", &[("boot", measured("boot-2"))]),
                answered("e", &[("boot", unknown(REASON_NOT_MEASURABLE))]),
            ],
            &[],
        );
        let observed = group_of(&groups, "a");
        assert!(observed.this_computer);
        assert_eq!(observed.state, GroupState::Observed);
        let row = |id: &str| observed.members.iter().find(|m| m.id == id).unwrap();
        assert_eq!(row("a").facts["boot"].shared_with, ["b", "c"]);
        assert_eq!(row("a").facts["account"].shared_with, ["c"]);
        assert_eq!(row("b").facts["boot"].shared_with, ["a", "c"]);
        assert!(row("b").facts["account"].shared_with.is_empty());
        assert!(observed.members.iter().all(|m| !m.disagrees));
        // The other groups are not this computer.
        for other in ["d", "e"] {
            let group = group_of(&groups, other);
            assert!(!group.this_computer, "{other}");
            assert!(group.members[0].facts["boot"].shared_with.is_empty());
        }
    }

    #[test]
    fn shared_with_and_the_measured_relation_rows_never_disagree() {
        let groups = compare(
            &[
                answered(
                    "a",
                    &[
                        ("boot", measured("b1")),
                        ("account", measured("x")),
                        ("rack", measured("r")),
                    ],
                ),
                answered(
                    "b",
                    &[
                        ("boot", measured("b1")),
                        ("account", measured("x")),
                        ("rack", unknown(REASON_NOT_MEASURABLE)),
                    ],
                ),
                answered(
                    "c",
                    &[
                        ("boot", measured("b1")),
                        ("account", measured("y")),
                        ("rack", measured("r")),
                    ],
                ),
            ],
            &[],
        );
        for group in &groups {
            for member in &group.members {
                for (relation, fact) in &member.facts {
                    let from_rows: Vec<String> = group
                        .relations
                        .iter()
                        .filter(|row| {
                            row.grade == LocalityGrade::Measured
                                && row.relation == *relation
                                && row.members.contains(&member.id)
                        })
                        .flat_map(|row| row.members.iter().filter(|id| **id != member.id).cloned())
                        .collect();
                    assert_eq!(fact.shared_with, from_rows, "{} {relation}", member.id);
                    if fact.grade == LocalityGrade::Unknown {
                        assert!(fact.shared_with.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn a_member_in_the_disagreeing_list_says_so() {
        let groups = compare(
            &[
                answered("a", &[("boot", measured("boot-1"))]),
                answered("b", &[("boot", measured("boot-1"))]),
                answered("c", &[("boot", measured("boot-2"))]),
            ],
            &[declaration("workstation", &["a", "b", "c"])],
        );
        let flags: Vec<(&str, bool)> = groups[0]
            .members
            .iter()
            .map(|m| (m.id.as_str(), m.disagrees))
            .collect();
        assert_eq!(flags, [("a", false), ("b", false), ("c", true)]);
        assert!(!groups[0].this_computer);
    }

    #[test]
    fn grades_have_the_meaning_of_proof_sources() {
        assert_eq!(
            LocalityGrade::Measured.proof_source(),
            lillux::process_control::ProofSource::Measured
        );
        assert_ne!(
            LocalityGrade::Unknown.proof_source(),
            lillux::process_control::ProofSource::Measured
        );
    }
}
