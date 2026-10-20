//! Candidate preparation under one existing principal-library lock. This owner
//! compiles ordinary origins; it does not allocate, publish or invent receipts.
use super::{RyeOsPrincipalStoreExt, compile_saved_surface_context, locked_principal_store};
use crate::browser_session::admission::RegisteredProjectSurfaceAdmission;
use anyhow::Result;
use ryeos_app::{
    handler_context::HandlerContext, handler_error::HandlerError, principal::LockedPrincipalStore,
    state::AppState,
};
use ryeos_client_base::surface::{
    opening::{SurfaceOpenRequest, SurfaceOpenSelection},
    view_sets::{
        LayoutSeedSpec, SavedViewMountRef, SavedViewSelectionSource, SavedViewSetTemplate,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(crate) struct PreparedReusableTab {
    pub candidate: crate::browser_session::BindingAttachmentCandidate,
    pub project: Option<RegisteredProjectSurfaceAdmission>,
}

pub(crate) struct PreparedReusableSurface {
    /// Keep this guard alive through final revalidation and atomic publication.
    pub store: LockedPrincipalStore,
    pub receipt: crate::browser_session::SavedSurfaceRequestIdentity,
    pub origin_candidate: crate::browser_session::BindingAttachmentCandidate,
    pub origin_project: Option<RegisteredProjectSurfaceAdmission>,
    pub tabs: Vec<PreparedReusableTab>,
}

pub(crate) async fn prepare_reusable_surface(
    request: &SurfaceOpenRequest,
    ctx: &HandlerContext,
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
) -> Result<PreparedReusableSurface> {
    request.validate().map_err(HandlerError::BadRequest)?;
    let bounds = super::super::ui_launch_mint::binding_request_bounds(state)?;
    if serde_json::to_vec(request)?.len() as u64 > bounds.max_request_bytes {
        return Err(HandlerError::BadRequest(
            "Surface opening data exceeds signed request bounds".into(),
        )
        .into());
    }
    let store = locked_principal_store(ctx, state).await?;
    if let SurfaceOpenSelection::Composed(composition) = &request.selection {
        let projects = store.load_projects()?;
        let (origin_candidate, origin_project) =
            compile_saved_surface_context(state, session, &composition.origin, &projects)?;
        let mut tabs = Vec::with_capacity(composition.tabs.len());
        let mut records = Vec::with_capacity(composition.tabs.len());
        for tab in &composition.tabs {
            let (candidate, project) =
                compile_saved_surface_context(state, session, &tab.origin, &projects)?;
            validate_reusable_tab_binding(
                &tab.template,
                &candidate,
                Some(&tab.fresh_subjects),
                bounds.max_request_bytes,
            )?;
            records.push(crate::browser_session::ReusableSurfaceTabRecord {
                saved_id: tab.template.id.clone(),
                origin: tab.origin.clone(),
                template: tab.template.clone(),
                fresh_subjects: tab.fresh_subjects.clone(),
            });
            tabs.push(PreparedReusableTab { candidate, project });
        }
        return Ok(PreparedReusableSurface {
            store,
            receipt: crate::browser_session::SavedSurfaceRequestIdentity {
                provenance:
                    ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance::Composed {
                        composition: composition.clone(),
                    },
                title: composition.title.clone(),
                origin: composition.origin.clone(),
                active_view_set_id: composition.active_view_set_id.clone(),
                body: crate::browser_session::SavedSurfaceRequestBody::Composed { tabs: records },
            },
            origin_candidate,
            origin_project,
            tabs,
        });
    }
    let SurfaceOpenSelection::Reusable {
        id,
        expected_revision,
        fresh_subjects,
    } = &request.selection
    else {
        return Err(HandlerError::BadRequest(
            "portable preparation requires a reusable or composed selection".into(),
        )
        .into());
    };
    let config = store.load_ui_config()?;
    if config.reusable_surface_library_revision != *expected_revision {
        return Err(
            HandlerError::Conflict("reusable Surface library revision changed".into()).into(),
        );
    }
    let record = config
        .reusable_surfaces
        .iter()
        .find(|record| &record.id == id)
        .cloned()
        .ok_or(HandlerError::NotFound)?;
    request
        .validate_reusable(&record)
        .map_err(HandlerError::BadRequest)?;
    let projects = store.load_projects()?;
    let (origin_candidate, origin_project) =
        compile_saved_surface_context(state, session, &record.origin, &projects)?;
    let mut tabs = Vec::with_capacity(record.view_sets.len());
    for template in &record.view_sets {
        let origin = record
            .tab_origins
            .get(&template.id)
            .ok_or_else(|| HandlerError::BadRequest("reusable tab has no exact origin".into()))?;
        let (candidate, project) =
            compile_saved_surface_context(state, session, origin, &projects)?;
        validate_reusable_tab_binding(
            template,
            &candidate,
            fresh_subjects.get(&template.id),
            bounds.max_request_bytes,
        )?;
        tabs.push(PreparedReusableTab { candidate, project });
    }
    let record_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&record)?));
    let receipt = crate::browser_session::SavedSurfaceRequestIdentity {
        provenance: ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance::Library {
            saved_id: record.id.clone(),
            library_revision: *expected_revision,
            record_digest,
        },
        title: record.title.clone(),
        origin: record.origin.clone(),
        active_view_set_id: record.active_view_set_id.clone(),
        body: crate::browser_session::SavedSurfaceRequestBody::Reusable {
            tabs: record
                .view_sets
                .iter()
                .map(
                    |template| crate::browser_session::ReusableSurfaceTabRecord {
                        saved_id: template.id.clone(),
                        origin: record.tab_origins[&template.id].clone(),
                        template: template.clone(),
                        fresh_subjects: fresh_subjects
                            .get(&template.id)
                            .cloned()
                            .unwrap_or_default(),
                    },
                )
                .collect(),
        },
    };
    Ok(PreparedReusableSurface {
        store,
        receipt,
        origin_candidate,
        origin_project,
        tabs,
    })
}

fn validate_reusable_tab_binding(
    template: &SavedViewSetTemplate,
    candidate: &crate::browser_session::BindingAttachmentCandidate,
    fresh_subjects: Option<&BTreeMap<String, BTreeMap<String, Value>>>,
    bound: u64,
) -> Result<()> {
    fn refs(layout: &LayoutSeedSpec, out: &mut Vec<String>) {
        match layout {
            LayoutSeedSpec::Group { views, .. } => {
                out.extend(views.iter().map(|view| view.0.clone()))
            }
            LayoutSeedSpec::Split { first, second, .. } => {
                refs(first, out);
                refs(second, out);
            }
        }
    }
    let mut views = Vec::new();
    if let Some(root) = &template.composition.root {
        refs(root, &mut views);
    }
    let parsed = ryeos_client_base::ui::content::views_from_surface(Some(
        &serde_json::json!({"views":candidate.presentation.views}),
    ));
    for view_ref in &views {
        if !matches!(
            candidate.compiled_binding.binding.views.get(view_ref),
            Some(ryeos_api::surface_views::EmbeddedViewIdentity::Resolved { .. })
        ) {
            return Err(HandlerError::Forbidden(
                "reusable tab View is outside its exact admitted closure".into(),
            )
            .into());
        }
    }
    for relationship in &template.relationships {
        let SavedViewMountRef::Tile { index } = &relationship.mount;
        let view = views
            .get(*index)
            .and_then(|reference| parsed.get(reference))
            .ok_or_else(|| {
                HandlerError::Forbidden("reusable subject mount has no exact admitted View".into())
            })?;
        ryeos_client_base::ui::composition_subjects::validate_portable_subject(
            &relationship.source,
            view,
            bound,
        )
        .map_err(HandlerError::Conflict)?;
        if let SavedViewSelectionSource::RequiredSubject { input, .. } = &relationship.source
            && let Some(values) = fresh_subjects.and_then(|subjects| subjects.get(input))
        {
            validate_fresh_subject(view, values, bound)?;
        }
    }
    Ok(())
}

fn validate_fresh_subject(
    view: &ryeos_client_base::ui::content::ViewBinding,
    values: &BTreeMap<String, Value>,
    bound: u64,
) -> Result<()> {
    if bound == 0
        || values
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            != ryeos_client_base::ui::attachment::selection_dependencies(view)
        || values.values().any(Value::is_null)
        || serde_json::to_vec(values)?.len() as u64 > bound
    {
        return Err(HandlerError::BadRequest(
            "fresh reusable subject contradicts exact View dependencies or bounds".into(),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn supplied_fresh_subject_requires_exact_current_dependencies_and_bytes() {
        let view = serde_json::from_value::<ryeos_client_base::ui::content::ViewBinding>(json!({
            "widget":"rows","sources":{"default":{"ref":"service:test/work",
                "params":{"thread":"@facet:selection.work.thread"}}}
        }))
        .unwrap();
        let mut values = BTreeMap::from([("selection.work.thread".into(), json!("T-current"))]);
        let bound = serde_json::to_vec(&values).unwrap().len() as u64;
        validate_fresh_subject(&view, &values, bound).unwrap();
        assert!(validate_fresh_subject(&view, &values, bound - 1).is_err());
        assert!(validate_fresh_subject(&view, &values, 0).is_err());
        values.insert("selection.work.thread".into(), Value::Null);
        assert!(validate_fresh_subject(&view, &values, 65536).is_err());
        values.insert("selection.work.thread".into(), json!("T-current"));
        values.insert("selection.other".into(), json!("ambient"));
        assert!(validate_fresh_subject(&view, &values, 65536).is_err());
    }
}
