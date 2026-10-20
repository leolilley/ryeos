//! Pure admitted particular-subject projection. This is metadata, not authority
//! or a seat write. Later durable explicit selection (including null) wins.
use super::attachment::{SelectionAttachment, selection_dependencies};
use super::content::ViewBinding;
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};
use crate::surface::view_sets::{
    ResolvedParticularWork, SavedViewMountRef, SavedViewSelectionSource, SavedViewSetTemplate,
    validate_saved_view_set_templates,
};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct ParticularSubjectMount {
    pub required_input: Option<String>,
    pub attachment: SelectionAttachment,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParticularSubjectPlan {
    pub mounts: BTreeMap<RyeOsViewInstanceKey, ParticularSubjectMount>,
    pub work_selection: Option<Value>,
}

/// Validate portable relationship metadata against its exact admitted View.
/// Required subjects remain unresolved execution fences; this does not supply
/// values or resolve a FollowSet target. The causal composition owner does that.
pub fn validate_portable_subject(
    source: &SavedViewSelectionSource,
    binding: &ViewBinding,
    max_subject_bytes: u64,
) -> Result<(), String> {
    if max_subject_bytes == 0 {
        return Err("saved subject bound absent".into());
    }
    let encoded = serde_json::to_vec(source).map_err(|_| "saved subject encoding failed")?;
    if encoded.len() as u64 > max_subject_bytes {
        return Err("saved subject metadata exceeds admitted bound".into());
    }
    if let SavedViewSelectionSource::RequiredSubject { input, facets } = source
        && (input.trim().is_empty()
            || input.chars().any(char::is_control)
            || facets.is_empty()
            || selection_dependencies(binding) != facets.iter().cloned().collect()
            || facets
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != facets.len())
    {
        return Err("saved subject contradicts current View dependencies".into());
    }
    Ok(())
}

pub fn particular_subject_values(
    template: &SavedViewSetTemplate,
    work: Option<&ResolvedParticularWork>,
) -> BTreeMap<String, BTreeMap<String, Value>> {
    let Some(work) = work else {
        return BTreeMap::new();
    };
    template
        .relationships
        .iter()
        .filter_map(|relationship| {
            let SavedViewSelectionSource::RequiredSubject { input, facets } = &relationship.source
            else {
                return None;
            };
            let values = facets
                .iter()
                .filter_map(|facet| {
                    let value = match facet.as_str() {
                        "selection.work" => {
                            serde_json::json!({"thread":work.thread,"chain_root":work.chain_root})
                        }
                        "selection.work.thread" => Value::String(work.thread.clone()),
                        "selection.work.chain_root" => Value::String(work.chain_root.clone()),
                        _ => return None,
                    };
                    Some((facet.clone(), value))
                })
                .collect();
            Some((input.clone(), values))
        })
        .collect()
}

/// `ordered_mounts` comes from the admitted tab's retained mount manifest, not
/// an independently allocated or ref-prettified reconstruction.
pub fn project_particular_subjects(
    template: &SavedViewSetTemplate,
    work: Option<&ResolvedParticularWork>,
    view_set_id: ViewSetId,
    ordered_mounts: &[(RyeOsViewInstanceKey, String)],
    views: &BTreeMap<String, ViewBinding>,
    max_request_bytes: u64,
) -> Result<ParticularSubjectPlan, String> {
    let subjects = particular_subject_values(template, work);
    let mut plan = project_saved_subjects(
        template,
        &subjects,
        false,
        &BTreeMap::new(),
        view_set_id,
        ordered_mounts,
        views,
        max_request_bytes,
    )?;
    plan.work_selection =
        work.map(|work| serde_json::json!({"thread":work.thread,"chain_root":work.chain_root}));
    Ok(plan)
}

/// Fold portable subjects using only the exact admitted collection's tab map.
/// Missing reusable inputs stay unresolved; supplied inputs never use old pins.
pub fn project_saved_subjects(
    template: &SavedViewSetTemplate,
    subjects: &BTreeMap<String, BTreeMap<String, Value>>,
    allow_unresolved: bool,
    collection: &BTreeMap<String, ViewSetId>,
    view_set_id: ViewSetId,
    ordered_mounts: &[(RyeOsViewInstanceKey, String)],
    views: &BTreeMap<String, ViewBinding>,
    max_request_bytes: u64,
) -> Result<ParticularSubjectPlan, String> {
    validate_saved_view_set_templates(std::slice::from_ref(template))?;
    if ordered_mounts.len() != template.relationships.len() {
        return Err("saved subject mount inventory differs from its template".into());
    }
    if max_request_bytes == 0 {
        return Err("saved subject bound absent".into());
    }
    if subjects.keys().any(|input| !template.relationships.iter().any(|relationship|
        matches!(&relationship.source, SavedViewSelectionSource::RequiredSubject { input: expected, .. } if expected == input))) {
        return Err("saved subject input is undeclared".into());
    }
    let mut mounts = BTreeMap::new();
    for relationship in &template.relationships {
        let SavedViewMountRef::Tile { index } = &relationship.mount;
        let (instance, view_ref) = ordered_mounts
            .get(*index)
            .ok_or("saved subject mount absent")?;
        let binding = views.get(view_ref).ok_or("saved subject View absent")?;
        validate_portable_subject(&relationship.source, binding, max_request_bytes)?;
        let (required_input, attachment) = match &relationship.source {
            SavedViewSelectionSource::FollowOwnSet => {
                (None, SelectionAttachment::FollowViewSet { view_set_id })
            }
            SavedViewSelectionSource::FollowSet { saved_view_set_id } => (
                None,
                SelectionAttachment::FollowViewSet {
                    view_set_id: *collection
                        .get(saved_view_set_id)
                        .ok_or("saved cross-set subject lacks exact collection owner")?,
                },
            ),
            SavedViewSelectionSource::RequiredSubject { input, facets } => {
                let Some(values) = subjects.get(input) else {
                    if !allow_unresolved {
                        return Err("saved subject unresolved".into());
                    }
                    if mounts
                        .insert(
                            instance.clone(),
                            ParticularSubjectMount {
                                required_input: Some(input.clone()),
                                attachment: SelectionAttachment::RequiredSubject {
                                    input: input.clone(),
                                    facets: facets.clone(),
                                },
                            },
                        )
                        .is_some()
                    {
                        return Err("saved subject mount repeated".into());
                    }
                    continue;
                };
                if selection_dependencies(binding) != facets.iter().cloned().collect()
                    || values.len() != facets.len()
                    || facets.iter().any(|facet| !values.contains_key(facet))
                    || values.values().any(Value::is_null)
                {
                    return Err("saved subject contradicts current View dependencies".into());
                }
                (
                    Some(input.clone()),
                    super::attachment::bounded_subject_pin(values.clone(), max_request_bytes)?,
                )
            }
        };
        if mounts
            .insert(
                instance.clone(),
                ParticularSubjectMount {
                    required_input,
                    attachment,
                },
            )
            .is_some()
        {
            return Err("saved subject mount repeated".into());
        }
    }
    Ok(ParticularSubjectPlan {
        mounts,
        work_selection: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn portable_required_subject_is_an_exact_bounded_unresolved_relationship() {
        let binding = ViewBinding {
            body: json!({"work":"@facet:selection.work"}),
            ..Default::default()
        };
        let source = SavedViewSelectionSource::RequiredSubject {
            input: "work_subject".into(),
            facets: vec!["selection.work".into()],
        };
        assert!(validate_portable_subject(&source, &binding, 4096).is_ok());
        assert!(validate_portable_subject(&source, &binding, 0).is_err());
        assert!(validate_portable_subject(&source, &binding, 1).is_err());
        assert!(validate_portable_subject(&source, &ViewBinding::default(), 4096).is_err());
        let duplicate = SavedViewSelectionSource::RequiredSubject {
            input: "work_subject".into(),
            facets: vec!["selection.work".into(), "selection.work".into()],
        };
        assert!(validate_portable_subject(&duplicate, &binding, 4096).is_err());
    }

    #[test]
    fn subject_projection_is_exact_bounded_and_requires_resolution() {
        let template: SavedViewSetTemplate = serde_json::from_value(json!({
            "id":"work","name":"Work","composition":{"schema":2,"id":"work","title":"Work",
                "root":{"type":"group","views":["view:test/work"],"active":0}},
            "relationships":[{"mount":{"kind":"tile","index":0},
                "source":{"mode":"required_subject","input":"work_subject","facets":["selection.work"]}}]
        })).unwrap();
        let instance = RyeOsViewInstanceKey::surface_tile(
            &crate::ids::PresentationId::try_new("00000000-0000-4000-8000-000000000001").unwrap(),
            &crate::ids::SurfaceInstanceId::try_new("00000000-0000-4000-8000-000000000002")
                .unwrap(),
            crate::ids::TileId::new(17),
        );
        let mounts = vec![(instance.clone(), "view:test/work".into())];
        let mut views = BTreeMap::from([(
            "view:test/work".into(),
            ViewBinding {
                body: json!({"work":"@facet:selection.work"}),
                ..Default::default()
            },
        )]);
        let work = ResolvedParticularWork {
            thread: "T-head".into(),
            chain_root: "T-root".into(),
        };
        let plan = project_particular_subjects(
            &template,
            Some(&work),
            ViewSetId::new(9),
            &mounts,
            &views,
            4096,
        )
        .unwrap();
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[&instance].required_input.as_deref(),
            Some("work_subject")
        );
        assert_eq!(
            plan.work_selection,
            Some(json!({"thread":"T-head","chain_root":"T-root"}))
        );
        let SelectionAttachment::Pinned {
            values,
            fingerprint,
        } = &plan.mounts[&instance].attachment
        else {
            panic!("exact resolved pin")
        };
        assert_eq!(
            values["selection.work"],
            plan.work_selection.clone().unwrap()
        );
        assert_eq!(fingerprint.len(), 64);
        let unresolved = project_saved_subjects(
            &template,
            &BTreeMap::new(),
            true,
            &BTreeMap::new(),
            ViewSetId::new(9),
            &mounts,
            &views,
            4096,
        )
        .unwrap();
        assert!(matches!(
            &unresolved.mounts[&instance].attachment,
            SelectionAttachment::RequiredSubject { .. }
        ));
        assert!(unresolved.work_selection.is_none());
        let fresh = BTreeMap::from([(
            "work_subject".into(),
            BTreeMap::from([(
                "selection.work".into(),
                json!({"thread":"T-fresh","chain_root":"T-fresh-root"}),
            )]),
        )]);
        let fresh_plan = project_saved_subjects(
            &template,
            &fresh,
            true,
            &BTreeMap::new(),
            ViewSetId::new(9),
            &mounts,
            &views,
            4096,
        )
        .unwrap();
        let SelectionAttachment::Pinned { values, .. } = &fresh_plan.mounts[&instance].attachment
        else {
            panic!("fresh supplied subject is pinned");
        };
        assert_eq!(values["selection.work"]["thread"], "T-fresh");
        let mut invalid = fresh;
        invalid
            .get_mut("work_subject")
            .unwrap()
            .insert("selection.work".into(), Value::Null);
        assert!(
            project_saved_subjects(
                &template,
                &invalid,
                true,
                &BTreeMap::new(),
                ViewSetId::new(9),
                &mounts,
                &views,
                4096,
            )
            .is_err()
        );
        assert!(
            project_particular_subjects(&template, None, ViewSetId::new(9), &mounts, &views, 4096)
                .is_err()
        );
        assert!(
            project_particular_subjects(
                &template,
                Some(&work),
                ViewSetId::new(9),
                &mounts,
                &views,
                1
            )
            .is_err()
        );
        assert!(
            project_particular_subjects(
                &template,
                Some(&work),
                ViewSetId::new(9),
                &[],
                &views,
                4096
            )
            .is_err()
        );
        views.get_mut("view:test/work").unwrap().body = Value::Null;
        assert!(
            project_particular_subjects(
                &template,
                Some(&work),
                ViewSetId::new(9),
                &mounts,
                &views,
                4096
            )
            .is_err()
        );
    }
}
