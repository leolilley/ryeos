//! `ui.ryeos.node.machines` — the node's locality groups as flat rows for the
//! machines view.
//!
//! The groups come from the same comparison `service:node/locality/groups`
//! runs, and the caller needs that service's capability: this service never
//! shows more than the node service would. Each group becomes one row and each
//! of its members a row beneath it (`parent` names the group row), so a table
//! with a hierarchy can draw the nesting without reading arrays.
//!
//! Every sentence comes from the signed config `config:ryeos/ui/machines`,
//! keyed on the stable state, answer, grade, relation and reason keys. A key
//! the config does not list is shown as itself. The raw keys stay on every row
//! so the view's tone maps, and anything drawn later, can key on them. This
//! module decides no meaning: it carries what the comparison said.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_effect_contract::locality::{
    LocalityGrade, LocalityGroup, LocalityGroups, MemberAnswer, MemberKind, MemberRow,
};
use ryeos_executor::executor::ServiceAvailability;

pub const CONFIG_REF: &str = "config:ryeos/ui/machines";
const CONFIG_PATH: &str = "ryeos/ui/machines.yaml";
const SCHEMA: &str = "ryeos.ui.node.machines.v1";

/// What a caller needs to see locality at all: the node service's own
/// capability.
pub const NODE_CAPABILITY: &str = "ryeos.execute.service.node/locality/groups";

/// The key a member row carries as its status when its boot disagrees with the
/// rest of a declared machine.
const STATUS_DISAGREES: &str = "disagrees";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Templates {
    pub did_not_answer: String,
    pub fact_shared: String,
    pub fact_measured: String,
    pub fact_unknown: String,
    pub fact_unknown_bare: String,
    pub fact_line: String,
    pub relation_shared: String,
    pub relation_unknown: String,
    pub relation_unknown_bare: String,
    pub disagreeing: String,
    pub declared_missing: String,
}

/// The signed wording, keyed on the comparison's stable keys.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesWords {
    pub list_separator: String,
    pub line_separator: String,
    pub this_computer: String,
    pub states: BTreeMap<String, String>,
    pub answers: BTreeMap<String, String>,
    pub kinds: BTreeMap<String, String>,
    pub relations: BTreeMap<String, String>,
    pub reasons: BTreeMap<String, String>,
    pub templates: Templates,
}

impl MachinesWords {
    /// Parse a verified config body. The envelope fields are dropped first;
    /// any other unknown field is refused.
    pub fn from_value(mut value: Value) -> Result<Self, String> {
        if let Some(object) = value.as_object_mut() {
            for envelope in ["category", "version", "description"] {
                object.remove(envelope);
            }
        }
        let words: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        let texts = words
            .states
            .values()
            .chain(words.answers.values())
            .chain(words.kinds.values())
            .chain(words.relations.values())
            .chain(words.reasons.values())
            .chain([&words.this_computer]);
        if texts.into_iter().any(|text| text.is_empty()) {
            return Err("machines wording has an empty text".into());
        }
        Ok(words)
    }

    /// The text for `key` in `map`, or the key itself.
    fn text<'a>(map: &'a BTreeMap<String, String>, key: &'a str) -> &'a str {
        map.get(key).map_or(key, String::as_str)
    }

    fn state(&self, key: &str) -> String {
        Self::text(&self.states, key).to_owned()
    }

    fn answer(&self, key: &str) -> String {
        Self::text(&self.answers, key).to_owned()
    }

    fn relation(&self, key: &str) -> String {
        Self::text(&self.relations, key).to_owned()
    }

    fn reason(&self, key: &str) -> String {
        Self::text(&self.reasons, key).to_owned()
    }

    fn names(&self, names: &[String]) -> String {
        names.join(&self.list_separator)
    }
}

/// Fill `{placeholder}` markers in a template.
fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .fold(template.to_owned(), |text, (key, value)| {
            text.replace(&format!("{{{key}}}"), value)
        })
}

fn kind_key(kind: MemberKind) -> &'static str {
    match kind {
        MemberKind::This => "this",
        MemberKind::Remote => "remote",
        MemberKind::InstalledNotConnected => "installed_not_connected",
    }
}

fn grade_key(grade: LocalityGrade) -> &'static str {
    match grade {
        LocalityGrade::Measured => "measured",
        LocalityGrade::Unknown => "unknown",
    }
}

fn state_key(group: &LocalityGroup) -> String {
    serde_json::to_value(group.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The name a node id goes by within one group; an id nobody names is shown as
/// itself.
fn name_of<'a>(group: &'a LocalityGroup, id: &'a str) -> &'a str {
    group
        .members
        .iter()
        .find(|member| member.id == id)
        .map_or(id, |member| member.name.as_str())
}

fn names_of(group: &LocalityGroup, ids: &[String]) -> Vec<String> {
    ids.iter().map(|id| name_of(group, id).to_owned()).collect()
}

fn group_row(group: &LocalityGroup, as_of: &str, words: &MachinesWords) -> Value {
    let state = state_key(group);
    let all_names: Vec<String> = group.members.iter().map(|m| m.name.clone()).collect();
    let name = group
        .label
        .clone()
        .unwrap_or_else(|| words.names(&all_names));
    let mut lines = Vec::new();
    for relation in &group.relations {
        let names = words.names(&names_of(group, &relation.members));
        let relation_name = words.relation(&relation.relation);
        lines.push(match (relation.grade, relation.reason.as_deref()) {
            (LocalityGrade::Measured, _) => fill(
                &words.templates.relation_shared,
                &[("relation", &relation_name), ("names", &names)],
            ),
            (LocalityGrade::Unknown, Some(reason)) => fill(
                &words.templates.relation_unknown,
                &[
                    ("relation", &relation_name),
                    ("names", &names),
                    ("reason", &words.reason(reason)),
                ],
            ),
            (LocalityGrade::Unknown, None) => fill(
                &words.templates.relation_unknown_bare,
                &[("relation", &relation_name), ("names", &names)],
            ),
        });
    }
    let mut row = Map::new();
    row.insert("id".into(), json!(group.key));
    row.insert("kind".into(), json!("group"));
    row.insert("name".into(), json!(name));
    row.insert("state".into(), json!(state));
    row.insert("status".into(), json!(state));
    row.insert("state_text".into(), json!(words.state(&state)));
    row.insert("this_computer".into(), json!(group.this_computer));
    row.insert(
        "place".into(),
        json!(if group.this_computer {
            words.this_computer.as_str()
        } else {
            ""
        }),
    );
    row.insert("as_of".into(), json!(as_of));
    row.insert("declared".into(), json!(group.label.is_some()));
    row.insert("disagreeing".into(), json!(group.disagreeing));
    row.insert("declared_missing".into(), json!(group.declared_missing));
    if let Some(label) = &group.label {
        row.insert("label".into(), json!(label));
    }
    if !lines.is_empty() {
        row.insert(
            "relations_text".into(),
            json!(lines.join(&words.line_separator)),
        );
    }
    if !group.disagreeing.is_empty() {
        let names = words.names(&names_of(group, &group.disagreeing));
        row.insert(
            "disagreeing_text".into(),
            json!(fill(&words.templates.disagreeing, &[("names", &names)])),
        );
    }
    if !group.declared_missing.is_empty() {
        let names = words.names(&group.declared_missing);
        row.insert(
            "missing_text".into(),
            json!(fill(
                &words.templates.declared_missing,
                &[("names", &names)]
            )),
        );
    }
    Value::Object(row)
}

fn member_row(group: &LocalityGroup, member: &MemberRow, words: &MachinesWords) -> Value {
    let (answer_key, reason, as_of) = match &member.answer {
        MemberAnswer::Answered { as_of } => ("answered", None, Some(as_of)),
        MemberAnswer::DidNotAnswer { reason, as_of } => {
            ("did_not_answer", Some(reason.as_str()), Some(as_of))
        }
        MemberAnswer::NotGranted { as_of } => ("not_granted", None, Some(as_of)),
        MemberAnswer::NotAsked => ("not_asked", None, None),
    };
    let status = if member.disagrees {
        STATUS_DISAGREES
    } else {
        answer_key
    };
    let state_text = match reason {
        Some(reason) if !member.disagrees => fill(
            &words.templates.did_not_answer,
            &[
                ("answer", &words.answer(answer_key)),
                ("reason", &words.reason(reason)),
            ],
        ),
        _ => words.answer(status),
    };
    let mut facts = Map::new();
    let mut fact_lines = Vec::new();
    for (relation, fact) in &member.facts {
        let shared = names_of(group, &fact.shared_with);
        let text = match (fact.grade, fact.reason.as_deref()) {
            (LocalityGrade::Measured, _) if !shared.is_empty() => fill(
                &words.templates.fact_shared,
                &[("names", &words.names(&shared))],
            ),
            (LocalityGrade::Measured, _) => words.templates.fact_measured.clone(),
            (LocalityGrade::Unknown, Some(reason)) => fill(
                &words.templates.fact_unknown,
                &[("reason", &words.reason(reason))],
            ),
            (LocalityGrade::Unknown, None) => words.templates.fact_unknown_bare.clone(),
        };
        fact_lines.push(fill(
            &words.templates.fact_line,
            &[("relation", &words.relation(relation)), ("text", &text)],
        ));
        let mut entry = Map::new();
        entry.insert("text".into(), json!(text));
        entry.insert("grade".into(), json!(grade_key(fact.grade)));
        if let Some(reason) = &fact.reason {
            entry.insert("reason".into(), json!(reason));
        }
        entry.insert("shared_with".into(), json!(fact.shared_with));
        entry.insert("shared_with_names".into(), json!(shared));
        facts.insert(relation.clone(), Value::Object(entry));
    }
    let mut row = Map::new();
    row.insert("id".into(), json!(format!("{}/{}", group.key, member.id)));
    row.insert("parent".into(), json!(group.key));
    row.insert("kind".into(), json!("member"));
    row.insert("node_id".into(), json!(member.id));
    row.insert("name".into(), json!(member.name));
    row.insert("member_kind".into(), json!(kind_key(member.kind)));
    row.insert(
        "place".into(),
        json!(MachinesWords::text(&words.kinds, kind_key(member.kind))),
    );
    row.insert("answer".into(), json!(answer_key));
    row.insert("status".into(), json!(status));
    row.insert("state_text".into(), json!(state_text));
    row.insert("declared".into(), json!(member.declared));
    row.insert("disagrees".into(), json!(member.disagrees));
    if let Some(reason) = reason {
        row.insert("reason".into(), json!(reason));
    }
    if let Some(as_of) = as_of {
        row.insert("as_of".into(), json!(as_of));
    }
    row.insert("facts".into(), Value::Object(facts));
    if !fact_lines.is_empty() {
        row.insert(
            "facts_text".into(),
            json!(fact_lines.join(&words.line_separator)),
        );
    }
    Value::Object(row)
}

/// The response body: one group row, then its member rows, for each group in
/// the comparison's order.
pub fn machines_response(groups: &LocalityGroups, words: &MachinesWords) -> Value {
    let mut rows = Vec::new();
    for group in &groups.groups {
        rows.push(group_row(group, &groups.as_of, words));
        for member in &group.members {
            rows.push(member_row(group, member, words));
        }
    }
    json!({
        "schema": SCHEMA,
        "as_of": groups.as_of,
        "rows": rows,
    })
}

/// Refuse a caller that does not hold the node service's capability.
pub fn require_node_capability(ctx: &HandlerContext) -> Result<(), HandlerError> {
    if ctx
        .scopes
        .iter()
        .any(|granted| ryeos_runtime::authorizer::cap_matches(granted, NODE_CAPABILITY))
    {
        Ok(())
    } else {
        Err(HandlerError::Forbidden(format!(
            "machines need the {NODE_CAPABILITY} capability"
        )))
    }
}

/// Resolve and verify the signed machines wording.
pub fn load_words(state: &AppState) -> Result<MachinesWords> {
    let value = super::ui_attention::load_signed_config(state, CONFIG_PATH, CONFIG_REF)?;
    Ok(MachinesWords::from_value(value).map_err(HandlerError::Internal)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesRequest {}

pub async fn handle_machines(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;
    require_node_capability(&ctx)?;
    let _req: MachinesRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let words = load_words(&state)?;
    let groups = ryeos_api::handlers::node_locality_groups::compare_groups(ctx, state).await?;
    Ok(machines_response(&groups, &words))
}

pub const MACHINES_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/node/machines",
    endpoint: "ui.ryeos.node.machines",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[NODE_CAPABILITY],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_machines(params, ctx, state).await })
    },
};
