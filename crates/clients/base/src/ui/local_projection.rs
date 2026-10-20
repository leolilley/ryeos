//! Closed, read-only presentation metadata projected through ordinary Views.
//!
//! A projection is not an executable source or an authority grant. Interactive
//! records must be regenerated against the exact mounted binding before their
//! canonical Rust intent can be activated.

use super::view_model::{RyeOsRowVm, RyeOsTone, RyeOsViewVm};
use super::{content::ViewBinding, event::RyeOsUiIntent, model::RyeOsCore};
use crate::ids::RyeOsViewInstanceKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewLocalProjection {
    Launcher,
    Shortcuts,
    SessionDetails,
}

#[derive(Serialize)]
struct Candidate {
    id: String,
    label: String,
    hint: String,
    enabled: bool,
    intent: Option<RyeOsUiIntent>,
}

fn candidate(
    label: String,
    hint: String,
    enabled: bool,
    intent: Option<RyeOsUiIntent>,
) -> Option<Candidate> {
    let id = super::seat_digest::seat_payload_digest(&serde_json::json!({
        "domain":"ryeos.ui.local-projection.record.v1", "label":label,"hint":hint,"intent":intent,
    }))
    .ok()?;
    Some(Candidate {
        id,
        label,
        hint,
        enabled,
        intent,
    })
}

fn global_or_exact_command(
    intent: &RyeOsUiIntent,
    has_destination: bool,
    has_surface_origin: bool,
) -> bool {
    match intent {
        RyeOsUiIntent::ToggleBackdropBreak
        | RyeOsUiIntent::ToggleDock { .. }
        | RyeOsUiIntent::ToggleTopStatusBar
        | RyeOsUiIntent::ToggleBottomStatusBar
        | RyeOsUiIntent::OpenDialog { .. }
        | RyeOsUiIntent::OpenNewPresentationDocument
        | RyeOsUiIntent::SelectSurface { .. }
        | RyeOsUiIntent::CloseSurface { .. }
        | RyeOsUiIntent::RenameSurface { .. }
        | RyeOsUiIntent::ReleaseBindingAttachment { .. } => true,
        RyeOsUiIntent::NewViewSet
        | RyeOsUiIntent::DuplicateViewSet { .. }
        | RyeOsUiIntent::CloseViewSet { .. } => has_destination,
        RyeOsUiIntent::InvokeAffordance { .. }
        | RyeOsUiIntent::InspectItem { .. }
        | RyeOsUiIntent::InspectThread { .. }
        | RyeOsUiIntent::InspectSummary { .. }
        | RyeOsUiIntent::ReadFile { .. }
        | RyeOsUiIntent::PinViewSelection { .. }
        | RyeOsUiIntent::SupplyRequiredSubject { .. }
        | RyeOsUiIntent::OpenPinnedViewAlongside { .. }
        | RyeOsUiIntent::FollowViewSetSelection { .. }
        | RyeOsUiIntent::MoveTileToViewSet { .. }
        | RyeOsUiIntent::MoveTileToSurfaceViewSet { .. }
        | RyeOsUiIntent::PromoteTileToMaster { .. }
        | RyeOsUiIntent::MoveTileBeside { .. }
        | RyeOsUiIntent::MoveTileToGroup { .. } => has_destination && has_surface_origin,
        // Commands dependent on the latest input route/focus are not safe
        // substitutes for an exactly captured operation.
        _ => false,
    }
}

/// A local projection cannot be its own contextual row source: asking for its
/// selected row would project it again. Keep global commands available while
/// failing closed on contextual commands for that focus.
fn command_focus<'a>(
    core: &RyeOsCore,
    focused: Option<&'a RyeOsViewInstanceKey>,
) -> Option<&'a RyeOsViewInstanceKey> {
    focused.filter(|instance| {
        core.mounted_view_ref(instance)
            .and_then(|view_ref| core.binding_for_instance(instance, view_ref))
            .is_some_and(|binding| binding.local_projection.is_none())
    })
}

fn candidates(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    binding: &ViewBinding,
) -> Option<(Vec<Candidate>, String)> {
    let composition = core.presentation.as_ref()?;
    let mount = composition.mount(instance)?;
    if !mount.visible() {
        return None;
    }
    let descriptor = core.binding_attachment_for_instance(instance)?;
    let projection = binding.local_projection?;
    let dialog = composition
        .dialogs()
        .values()
        .find(|dialog| dialog.view().instance_key() == instance);
    let destination = dialog.and_then(|dialog| dialog.insertion());
    // The shared dialog retains its invoking destination across tab/Surface
    // focus changes. Keep the frame visible, but suppress generic actions
    // whose intent would otherwise dispatch into the newly active tab rather
    // than that captured destination. Saved-set operations separately use the
    // exact retained insertion coordinate.
    let destination_active = destination.is_some_and(|destination| {
        composition.active_surface().is_some_and(|surface| {
            surface.id() == &destination.surface_instance_id
                && surface
                    .active_view_set()
                    .is_some_and(|set| set.id == destination.view_set_id)
        })
    });
    let origin = dialog.and_then(|dialog| dialog.context_origin());
    // A tab switch hides, but does not unmount, the captured origin. That
    // cannot authorize origin-specific actions in the new tab, yet it must
    // not erase the shared dialog's frame or its global actions. An origin
    // that has actually been removed still fails closed.
    let origin_visible = match origin.map(|key| composition.mount(key)) {
        Some(None) => return None,
        Some(Some(mount)) => mount.visible(),
        None => false,
    };
    let has_surface_origin = origin.is_some_and(|origin| {
        matches!(
            composition.mount(origin),
            Some(super::presentation_state::MountRef::SurfaceTile { .. })
        )
    }) && origin_visible;
    let mut records = Vec::new();
    if destination.is_some() && !destination_active {
        records.push(candidate(
            "Captured tab is not active".into(),
            "Switch back to the captured tab or reopen this dialog to choose a new destination"
                .into(),
            false,
            None,
        )?);
    }
    match projection {
        ViewLocalProjection::SessionDetails => return None,
        ViewLocalProjection::Shortcuts => {
            for entry in super::keymap::shortcut_entries() {
                records.push(candidate(entry.keys, entry.description, false, None)?);
            }
        }
        ViewLocalProjection::Launcher => {
            if let Some(destination) = destination.filter(|_| destination_active) {
                let attachment_id = core.insertion_attachment_id(destination.view_set_id)?;
                let attachment = core.binding_attachments.get(attachment_id)?;
                let composed_opener = composition.dialogs().iter().find_map(|(id, dialog)| {
                    if dialog.surface_owner().is_some() || !composition.dialog_available(dialog) {
                        return None;
                    }
                    let view = core.binding_for_instance(
                        dialog.view().instance_key(),
                        dialog.view().view_ref(),
                    )?;
                    if !view.affordances.iter().any(|entry| {
                        entry.get("id").and_then(serde_json::Value::as_str)
                            == Some("open-composed-surface")
                    }) {
                        return None;
                    }
                    let descriptor =
                        core.binding_attachment_for_instance(dialog.view().instance_key())?;
                    Some((
                        id.clone(),
                        super::binding::UiBindingAttachmentCoordinate {
                            binding_attachment_id: descriptor.binding_attachment_id.clone(),
                            binding_generation: descriptor.binding_generation,
                            binding_digest: descriptor.binding_digest.clone(),
                        },
                    ))
                });
                let mut listed = std::collections::BTreeSet::new();
                for group in core.library_groups() {
                    for view_ref in group.refs {
                        if !listed.insert(view_ref.clone()) {
                            continue;
                        }
                        let Some(view) = attachment
                            .views
                            .get(&view_ref)
                            .filter(|view| view.degraded.is_none())
                        else {
                            continue;
                        };
                        let label = view
                            .title
                            .clone()
                            .or_else(|| view.name.clone())
                            .unwrap_or_else(|| view_ref.clone());
                        let missing = super::view_model::unsatisfied_facets(core, view);
                        records.push(candidate(
                            label.clone(),
                            if missing.is_empty() {
                                format!("{} · Open admitted View in the captured tab", group.title)
                            } else {
                                format!(
                                    "{} · Requires selection: {}",
                                    group.title,
                                    missing.join(", ")
                                )
                            },
                            missing.is_empty(),
                            Some(RyeOsUiIntent::OpenView {
                                view: crate::view_set::ViewSpec::bound(view_ref.clone()),
                            }),
                        )?);
                        let descriptor = core.binding_attachment(attachment_id)?;
                        records.push(candidate(
                            format!("{label} · New tab"),
                            format!(
                                "{} · Fresh composition; select any required subject anew",
                                group.title
                            ),
                            core.composition_write_available(),
                            Some(RyeOsUiIntent::OpenViewInNewTab {
                                surface_instance_id: destination.surface_instance_id.clone(),
                                view_set_id: destination.view_set_id,
                                binding: super::binding::UiBindingAttachmentCoordinate {
                                    binding_attachment_id: descriptor.binding_attachment_id.clone(),
                                    binding_generation: descriptor.binding_generation,
                                    binding_digest: descriptor.binding_digest.clone(),
                                },
                                view_ref: view_ref.clone(),
                            }),
                        )?);
                        if let Some((dialog_id, dialog_binding)) = &composed_opener {
                            records.push(candidate(
                                format!("{label} · New Surface"),
                                "Fresh subject selection in a separately admitted Surface".into(),
                                core.composition_write_available(),
                                Some(RyeOsUiIntent::OpenCompositionInNewSurface {
                                    surface_instance_id: destination.surface_instance_id.clone(),
                                    view_set_id: destination.view_set_id,
                                    binding: super::binding::UiBindingAttachmentCoordinate {
                                        binding_attachment_id: descriptor
                                            .binding_attachment_id
                                            .clone(),
                                        binding_generation: descriptor.binding_generation,
                                        binding_digest: descriptor.binding_digest.clone(),
                                    },
                                    view_ref: Some(view_ref),
                                    dialog_id: dialog_id.clone(),
                                    dialog_binding: dialog_binding.clone(),
                                }),
                            )?);
                        }
                    }
                }
                if let Some((dialog_id, dialog_binding)) = composed_opener {
                    let descriptor = core.binding_attachment(attachment_id)?;
                    if core.supports_independent_documents
                        && let Some(surface) = core.active_surface()
                    {
                        let can_open_document = core
                            .presentation
                            .as_ref()
                            .and_then(|presentation| presentation.dialogs().get(&dialog_id))
                            .and_then(|dialog| {
                                core.binding_for_instance(
                                    dialog.view().instance_key(),
                                    dialog.view().view_ref(),
                                )
                            })
                            .is_some_and(|view| {
                                view.affordances.iter().any(|entry| {
                                    entry.get("id").and_then(serde_json::Value::as_str)
                                        == Some("open-composed-document")
                                })
                            });
                        if can_open_document {
                            records.push(candidate(
                                    "Current Surface · New presentation".into(),
                                    "Prepare all tabs with fresh subjects, then open an explicit independent link".into(),
                                    core.composition_write_available(),
                                    Some(RyeOsUiIntent::OpenSurfaceInNewPresentation {surface_instance_id:surface.id().clone(),binding:surface.binding().clone(),dialog_id:dialog_id.clone(),dialog_binding:dialog_binding.clone()}),
                                )?);
                        }
                    }
                    records.push(candidate(
                        "Current tab · New Surface".into(),
                        "Reuse this layout without live pins, drafts or execution state".into(),
                        core.composition_write_available(),
                        Some(RyeOsUiIntent::OpenCompositionInNewSurface {
                            surface_instance_id: destination.surface_instance_id.clone(),
                            view_set_id: destination.view_set_id,
                            binding: super::binding::UiBindingAttachmentCoordinate {
                                binding_attachment_id: descriptor.binding_attachment_id.clone(),
                                binding_generation: descriptor.binding_generation,
                                binding_digest: descriptor.binding_digest.clone(),
                            },
                            view_ref: None,
                            dialog_id,
                            dialog_binding,
                        }),
                    )?);
                }
            }
            // Match the dialog's captured input focus. With no visible origin,
            // the former focused_input=None context fell back to the active
            // view-set focus; do that without copying the entire core/seat.
            let fallback = core.focused_view_instance_key_without_input_focus();
            let focused = command_focus(
                core,
                origin.filter(|_| origin_visible).or(fallback.as_ref()),
            );
            for command in super::view_model::available_command_items_with_focus(core, focused) {
                if global_or_exact_command(
                    &command.intent,
                    destination_active,
                    has_surface_origin && destination_active,
                ) {
                    records.push(candidate(
                        command.label,
                        command.hint,
                        command.enabled,
                        Some(command.intent),
                    )?);
                }
            }
            for (id, dialog) in composition.dialogs() {
                if dialog.view().instance_key() == instance || !composition.dialog_available(dialog)
                {
                    continue;
                }
                records.push(candidate(
                    dialog.title().to_owned(),
                    "Open admitted presentation dialog".into(),
                    true,
                    Some(RyeOsUiIntent::OpenDialog {
                        dialog_id: id.clone(),
                    }),
                )?);
            }
        }
    }
    let insertion_binding = destination
        .and_then(|destination| core.insertion_attachment_id(destination.view_set_id))
        .and_then(|id| core.binding_attachments.get(id))
        .map(|binding| &binding.descriptor);
    let guard = super::seat_digest::seat_payload_digest(&serde_json::json!({
        "domain":"ryeos.ui.local-projection.guard.v1", "mount":instance,
        "binding":descriptor,"destination":destination.map(|destination| serde_json::json!({"surface":destination.surface_instance_id,"view_set":destination.view_set_id})),"origin":origin,
        "origin_binding":origin.and_then(|origin| core.binding_attachment_for_instance(origin)),
        "origin_route":origin.map(|origin| core.route_for_instance(origin)),
        "origin_selection":origin.and_then(|origin| core.facet_value_for_instance(origin,"selection")),
        "insertion_binding":insertion_binding,"layout":core.layout_guard(),"records":records,
    })).ok()?;
    if let Some(input) = binding.input.as_ref().filter(|input| input.local_filter) {
        let key = core.input_key_for(instance.clone(), mount.view_ref(), input)?;
        let query = core
            .input_buffers
            .get(&key.storage_key())
            .map(|buffer| buffer.text.trim().to_lowercase())
            .unwrap_or_default();
        if !query.is_empty() {
            records.retain(|record| {
                format!("{} {}", record.label, record.hint)
                    .to_lowercase()
                    .contains(&query)
            });
        }
    }
    Some((records, guard))
}

pub(crate) fn activate(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    record_id: &str,
    guard: &str,
) -> Option<RyeOsUiIntent> {
    let view_ref = core.mounted_view_ref(instance)?;
    let binding = core.binding_for_instance(instance, view_ref)?;
    let (records, current) = candidates(core, instance, binding)?;
    if current != guard {
        return None;
    }
    records
        .into_iter()
        .find(|record| record.id == record_id && record.enabled)?
        .intent
}

pub(crate) fn project(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    binding: &ViewBinding,
) -> RyeOsViewVm {
    let title = binding
        .title
        .clone()
        .or_else(|| binding.name.clone())
        .unwrap_or_else(|| {
            core.mounted_view_ref(instance)
                .unwrap_or("Unavailable View")
                .to_owned()
        });
    if binding.local_projection == Some(ViewLocalProjection::SessionDetails) {
        return session_details(core, instance, title);
    }
    let Some((records, guard)) = candidates(core, instance, binding) else {
        return RyeOsViewVm::Placeholder {
            title,
            message: "The captured presentation context is no longer available.".into(),
        };
    };
    let cursor = match core.mounted_local_state(instance) {
        Some(crate::view_set::ViewLocalState::GenericList { cursor, .. }) => *cursor,
        _ => 0,
    };
    let total_rows = records.len();
    let selected = total_rows.checked_sub(1).map(|last| cursor.min(last));
    let rows = records
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            let enabled = record.enabled;
            RyeOsRowVm {
                id: record.id.clone(),
                cursor: index,
                glyph: (!enabled).then(|| "×".to_string()),
                primary: record.label,
                secondary: Some(if enabled {
                    record.hint
                } else {
                    format!("Unavailable — {}", record.hint)
                }),
                meta: None,
                kind: (!enabled).then(|| "unavailable".to_string()),
                intent: record.intent.filter(|_| enabled).map(|_| {
                    RyeOsUiIntent::ActivateLocalProjection {
                        instance_key: instance.clone(),
                        record_id: record.id,
                        projection_guard: guard.clone(),
                    }
                }),
                tone: RyeOsTone::Neutral,
                selected: selected == Some(index),
                expandable: false,
                expanded: false,
                detail: Vec::new(),
                changed_at_ms: None,
                changed_tone: None,
            }
        })
        .collect();
    RyeOsViewVm::Rows {
        title,
        source_status: None,
        columns: Vec::new(),
        total_rows,
        provenance: core.mounted_view_ref(instance).map(str::to_owned),
        affordance_hints: Vec::new(),
        rows,
    }
}

/// Inspect retained presentation metadata, not credentials or inferred node
/// identity. The serving node's identity is supplied by its own status source.
fn session_details(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    title: String,
) -> RyeOsViewVm {
    let context = (|| {
        let presentation = core.presentation.as_ref()?;
        let mount = presentation.mount(instance)?;
        if !mount.visible() {
            return None;
        }
        let attachment = core.binding_attachment_for_instance(instance)?;
        let root = core.binding_attachment(&presentation.root_binding().binding_attachment_id)?;
        if root.binding_generation != presentation.root_binding().binding_generation
            || root.binding_digest != presentation.root_binding().binding_digest
        {
            return None;
        }
        let session = core.data.session.as_ref()?;
        Some((presentation, attachment, root, session))
    })();
    let Some((presentation, attachment, root, session)) = context else {
        return RyeOsViewVm::Placeholder {
            title,
            message: "Authenticated presentation details are unavailable.".into(),
        };
    };
    let details = [
        (
            "Acting principal",
            session
                .user_principal_id
                .clone()
                .unwrap_or_else(|| "Unavailable".into()),
        ),
        ("Session", "Authenticated".into()),
        ("Presentation", presentation.id().as_str().to_owned()),
        ("Shared root", root.root_ref.clone()),
        ("View attachment", attachment.binding_attachment_id.clone()),
        (
            "Attachment generation",
            attachment.binding_generation.to_string(),
        ),
        (
            "Open Surfaces",
            presentation.ordered_surfaces().count().to_string(),
        ),
    ];
    let rows = details
        .into_iter()
        .enumerate()
        .map(|(cursor, (field, value))| RyeOsRowVm {
            id: format!("session-detail:{cursor}"),
            cursor,
            glyph: None,
            primary: field.into(),
            secondary: None,
            meta: None,
            kind: Some("detail".into()),
            intent: None,
            tone: RyeOsTone::Neutral,
            selected: false,
            expandable: false,
            expanded: false,
            detail: vec![super::view_model::RyeOsRowDetailVm {
                field: field.into(),
                value,
                tone: None,
            }],
            changed_at_ms: None,
            changed_tone: None,
        })
        .collect();
    RyeOsViewVm::Rows {
        title,
        source_status: None,
        columns: Vec::new(),
        total_rows: 7,
        provenance: core.mounted_view_ref(instance).map(str::to_owned),
        affordance_hints: Vec::new(),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support;

    fn fixture(shared_only: bool) -> (RyeOsCore, RyeOsViewInstanceKey) {
        fixture_with_surfaces(shared_only, false)
    }

    fn fixture_with_surfaces(
        shared_only: bool,
        second_surface: bool,
    ) -> (RyeOsCore, RyeOsViewInstanceKey) {
        fixture_with_projection(shared_only, second_surface, "launcher")
    }

    fn fixture_with_projection(
        shared_only: bool,
        second_surface: bool,
        projection: &str,
    ) -> (RyeOsCore, RyeOsViewInstanceKey) {
        let mut session = if shared_only {
            test_support::shared_session()
        } else {
            test_support::session()
        };
        if second_surface {
            let mut sibling = session.presentations[0].surfaces[0].clone();
            sibling.surface_instance_id = "00000000-0000-4000-8000-000000000099".into();
            sibling.ordinal = 2;
            sibling.title = "Second Surface".into();
            let descriptor = session
                .binding_attachments
                .iter()
                .find(|entry| entry.binding_attachment_id == sibling.binding.binding_attachment_id)
                .unwrap();
            let seeds = descriptor
                .presentation
                .definition
                .surface()
                .unwrap()
                .to_mount_seeds()
                .unwrap();
            let active = seeds.first().map(|seed| seed.id.clone());
            sibling.mount_manifest =
                super::super::composition_identity::UiSurfaceMountManifest::allocate(
                    crate::ids::PresentationId::try_new(
                        session.presentations[0].presentation_id.clone(),
                    )
                    .unwrap(),
                    crate::ids::SurfaceInstanceId::try_new(sibling.surface_instance_id.clone())
                        .unwrap(),
                    session.presentations[0].next_mount_id,
                    seeds
                        .into_iter()
                        .map(|seed| (seed, sibling.binding.clone()))
                        .collect(),
                    active.as_deref(),
                )
                .unwrap();
            session.presentations[0].next_mount_id = sibling.mount_manifest.next_mount_id;
            session.presentations[0].surfaces.push(sibling);
        }
        let root = session
            .binding_attachments
            .iter_mut()
            .find(|attachment| attachment.binding_attachment_id == session.root_attachment_id)
            .unwrap();
        root.presentation.views.insert(
            "view:ryeos/launcher".into(),
            serde_json::json!({
            "widget":"rows", "title":"Launch", "local_projection":projection,
            "input":{"id":"query","local_filter":true},
            }),
        );
        let mut core = test_support::core(session);
        core.focused_input = None;
        core.open_dialog("launcher");
        let key = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        (core, key)
    }

    #[test]
    fn session_details_work_without_surfaces_and_never_offer_execution() {
        let (mut core, key) = fixture_with_projection(true, false, "session_details");
        core.data.session.as_mut().unwrap().user_principal_id = Some("principal:actual".into());
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let RyeOsViewVm::Rows { rows, .. } = project(&core, &key, binding) else {
            panic!("ordinary session details must render");
        };
        assert!(
            rows.iter()
                .all(|row| row.intent.is_none() && row.kind.as_deref() == Some("detail"))
        );
        assert!(rows.iter().any(|row| {
            row.detail
                .iter()
                .any(|field| field.field == "Acting principal" && field.value == "principal:actual")
        }));
        assert!(rows.iter().any(|row| {
            row.detail
                .iter()
                .any(|field| field.field == "Open Surfaces" && field.value == "0")
        }));
        let encoded = serde_json::to_string(&rows).unwrap();
        assert!(!encoded.contains(&core.data.session.as_ref().unwrap().session_id));
        assert!(!encoded.contains("producer_incarnation"));
        assert!(!encoded.contains("token"));
        assert!(candidates(&core, &key, binding).is_none());
    }

    #[test]
    fn session_details_do_not_infer_identity_and_refuse_hidden_mounts() {
        let (mut core, key) = fixture_with_projection(false, false, "session_details");
        core.data.session.as_mut().unwrap().user_principal_id = None;
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap()
            .clone();
        let RyeOsViewVm::Rows { rows, .. } = project(&core, &key, &binding) else {
            panic!("ordinary session details must render");
        };
        assert!(rows.iter().any(|row| {
            row.detail
                .iter()
                .any(|field| field.field == "Acting principal" && field.value == "Unavailable")
        }));
        let _ = core.close_dialog(&key);
        assert!(matches!(
            project(&core, &key, &binding),
            RyeOsViewVm::Placeholder { .. }
        ));
    }

    #[test]
    fn session_details_refuse_missing_or_changed_exact_attachment() {
        let (mut core, key) = fixture_with_projection(true, false, "session_details");
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap()
            .clone();
        let id = core
            .binding_attachment_for_instance(&key)
            .unwrap()
            .binding_attachment_id
            .clone();
        let retained = core.binding_attachments.get_mut(&id).unwrap();
        retained.descriptor.binding_generation += 1;
        assert!(matches!(
            project(&core, &key, &binding),
            RyeOsViewVm::Placeholder { .. }
        ));
        core.binding_attachments.remove(&id);
        assert!(matches!(
            project(&core, &key, &binding),
            RyeOsViewVm::Placeholder { .. }
        ));
    }

    #[test]
    fn launcher_uses_exact_projected_surface_actions_and_close_guard() {
        let (mut core, key) = fixture_with_surfaces(false, true);
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (before, _) = candidates(&core, &key, binding).unwrap();
        assert!(
            !before
                .iter()
                .any(|record| matches!(record.intent, Some(RyeOsUiIntent::CloseSurface { .. })))
        );

        core.presentation_producer_ready(
            crate::ui::binding::UiPresentationProducer {
                presentation_id: test_support::FIXTURE_PRESENTATION_ID.into(),
                thread_id: "fixture-seat".into(),
                producer_incarnation: "a".repeat(64),
            },
            0,
        );
        let choices = crate::ui::view_model::build_view_model(&core)
            .session
            .surfaces;
        assert!(choices.iter().any(|choice| !choice.active));
        assert!(choices.iter().any(|choice| choice.close_intent.is_some()));
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &key, binding).unwrap();
        for choice in &choices {
            if !choice.active {
                let record = records
                    .iter()
                    .find(|record| record.intent.as_ref() == Some(&choice.select_intent))
                    .expect("other Surface selection is in the ordinary launcher");
                assert_eq!(
                    activate(&core, &key, &record.id, &guard),
                    Some(choice.select_intent.clone())
                );
            }
            if let Some(close) = &choice.close_intent {
                let record = records
                    .iter()
                    .find(|record| record.intent.as_ref() == Some(close))
                    .expect("only available exact Surface close is in the launcher");
                assert_eq!(
                    activate(&core, &key, &record.id, &guard),
                    Some(close.clone())
                );
            }
        }
    }

    #[test]
    fn launcher_activates_required_subject_from_captured_tile() {
        let mut session = test_support::session_with_surface(serde_json::json!({
            "name":"Subject fixture", "tiles":["view:test/subject"],
            "views":{"view:test/subject":{"widget":"rows","sources":{"default":{
                "ref":"service:test/subject","params":{"thread":"@facet:selection.work.thread"}
            }}}}
        }));
        let root = session
            .binding_attachments
            .iter_mut()
            .find(|attachment| attachment.binding_attachment_id == session.root_attachment_id)
            .unwrap();
        root.presentation.views.insert(
            "view:ryeos/launcher".into(),
            serde_json::json!({
                "widget": "rows", "title": "Launch", "local_projection": "launcher",
                "input": {"id": "query", "local_filter": true}
            }),
        );
        root.presentation.views.insert(
            "view:test/subject".into(),
            serde_json::json!({
                "widget": "rows",
                "sources": {"default": {
                    "ref": "service:test/subject",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }),
        );
        let mut core = test_support::core(session);
        let tile_id = core.active_view_set().unwrap().focused_tile;
        let source_set = core.active_view_set().unwrap().id;
        let origin = core.active_view_set().unwrap().tiles[&tile_id]
            .instance_key
            .clone();
        let selection_key =
            super::super::seat::selection_storage_key(source_set, "selection.work").unwrap();
        core.seat
            .append_facet(selection_key, serde_json::json!({"thread": "T-current"}));
        core.selection_attachments.insert(
            origin.clone(),
            super::super::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );
        core.focused_input = None;
        core.open_dialog("launcher");
        let dialog = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        let binding = core
            .binding_for_instance(&dialog, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &dialog, binding).unwrap();
        let record = records
            .iter()
            .find(|record| {
                matches!(
                    record.intent.as_ref(),
                    Some(RyeOsUiIntent::SupplyRequiredSubject { instance_key, source_view_set_id })
                        if instance_key == &origin && *source_view_set_id == source_set
                )
            })
            .expect("captured unresolved tile offers its exact subject action");
        assert!(record.enabled);
        assert_eq!(activate(&core, &dialog, &record.id, &guard), record.intent);
    }

    #[test]
    fn launcher_new_tab_uses_captured_binding_and_closed_dialog_rejects_it() {
        let (mut core, key) = fixture(false);
        let producer = test_support::claim_fixture_producer(&mut core);
        test_support::settle_pending_prefix(&mut core, &producer);
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &key, binding).unwrap();
        let row = records
            .iter()
            .find(|row| matches!(&row.intent, Some(RyeOsUiIntent::OpenViewInNewTab { .. })))
            .expect("admitted library View offers a new-tab action");
        let Some(RyeOsUiIntent::OpenViewInNewTab {
            surface_instance_id,
            view_set_id,
            binding,
            ..
        }) = &row.intent
        else {
            unreachable!()
        };
        assert_eq!(surface_instance_id, core.active_surface().unwrap().id());
        assert_eq!(*view_set_id, core.active_view_set().unwrap().id);
        assert_eq!(
            Some(binding.binding_attachment_id.as_str()),
            core.insertion_attachment_id(*view_set_id)
        );
        assert_eq!(activate(&core, &key, &row.id, &guard), row.intent);
        let row_id = row.id.clone();
        core.close_dialog(&key);
        assert!(activate(&core, &key, &row_id, &guard).is_none());
    }

    #[test]
    fn launcher_uses_authored_library_order_and_excludes_non_lenses() {
        let mut session = test_support::session_with_surface(serde_json::json!({
            "name":"library-order",
            "tiles":["view:test/home"],
            "library":[{"group":"Workbench","views":[
                "view:test/beta","view:test/home","view:test/scene",
                "view:test/input","view:test/needs-selection"
            ]}],
            "views":{
                "view:test/home":{"widget":"text","title":"Home","body":"Home"},
                "view:test/beta":{"widget":"text","title":"Beta","body":"Beta"},
                "view:test/scene":{"widget":"scene","title":"Backdrop"},
                "view:test/input":{"widget":"text","title":"Input only","input":{"id":"line"}},
                "view:test/needs-selection":{"widget":"text","title":"Needs selection","body":"@facet:selection.thread_id"}
            }
        }));
        let root_id = session.root_attachment_id.clone();
        let root = session
            .binding_attachments
            .iter_mut()
            .find(|attachment| attachment.binding_attachment_id == root_id)
            .unwrap();
        root.presentation.views.insert(
            "view:ryeos/launcher".into(),
            serde_json::json!({
                "widget":"rows","title":"Launch","local_projection":"launcher"
            }),
        );
        let mut core = test_support::core(session);
        core.open_dialog("launcher");
        let key = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &key, binding).unwrap();
        let views = records
            .iter()
            .filter(|record| matches!(record.intent, Some(RyeOsUiIntent::OpenView { .. })))
            .collect::<Vec<_>>();
        assert_eq!(
            views
                .iter()
                .map(|record| record.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Beta", "Home", "Needs selection"]
        );
        assert!(
            views
                .iter()
                .all(|record| record.hint.starts_with("Workbench · "))
        );
        assert!(!views[2].enabled);
        assert!(views[2].hint.contains("selection.thread_id"));
        assert!(activate(&core, &key, &views[2].id, &guard).is_none());
    }

    #[test]
    fn launcher_with_zero_surfaces_projects_only_existing_presentation_actions() {
        let (mut core, key) = fixture(true);
        test_support::claim_fixture_producer(&mut core);
        assert!(core.active_surface().is_none());
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, _) = candidates(&core, &key, binding).unwrap();
        assert!(!records.is_empty());
        assert!(records.iter().any(|record| matches!(
            &record.intent,
            Some(RyeOsUiIntent::OpenNewPresentationDocument)
        )));
        assert!(records.iter().all(|record| !matches!(
            record.intent,
            Some(
                RyeOsUiIntent::OpenView { .. }
                    | RyeOsUiIntent::NewViewSet
                    | RyeOsUiIntent::SubmitThreadCommand { .. }
            )
        )));
    }

    #[test]
    fn shared_launcher_reaches_ordinary_projects_surface_discovery_dialog() {
        let mut session = test_support::shared_session();
        let root_id = session.root_attachment_id.clone();
        let root = session
            .binding_attachments
            .iter_mut()
            .find(|attachment| attachment.binding_attachment_id == root_id)
            .unwrap();
        root.presentation.views.insert(
            "view:ryeos/launcher".into(),
            serde_json::json!({
                "widget":"rows", "local_projection":"launcher"
            }),
        );
        let crate::presentation::UiCompositionDefinition::SharedPresentation { config } =
            &mut root.presentation.definition
        else {
            panic!("fixture must have a shared presentation");
        };
        config.shared_presentation.dialogs.push(
            serde_json::from_value(serde_json::json!({
                "id":"projects", "title":"Projects and Surfaces",
                "view":"view:ryeos/projects/list"
            }))
            .unwrap(),
        );
        root.presentation.views.insert(
            "view:ryeos/projects/list".into(),
            serde_json::json!({
                "widget":"sections",
                "sources":{"default":{"ref":"service:projects/list"}}
            }),
        );
        let mut core = test_support::core(session);
        core.open_dialog("launcher");
        let key = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &key, binding).unwrap();
        let row = records
            .iter()
            .find(|row| row.label == "Projects and Surfaces")
            .expect("ordinary projects dialog remains reachable with no Surface");
        assert_eq!(
            activate(&core, &key, &row.id, &guard),
            Some(RyeOsUiIntent::OpenDialog {
                dialog_id: "projects".into()
            })
        );
        assert!(records.iter().all(|row| !row.label.contains("ARC")));
    }

    #[test]
    fn local_projection_focus_is_not_reprojected_as_a_contextual_row() {
        let (core, key) = fixture(false);
        assert!(command_focus(&core, Some(&key)).is_none());
        let commands = super::super::view_model::available_command_items_with_focus(
            &core,
            command_focus(&core, Some(&key)),
        );
        assert!(
            commands
                .iter()
                .any(|choice| matches!(choice.intent, RyeOsUiIntent::ToggleBackdropBreak))
        );
    }

    #[test]
    fn borrowed_captured_focus_matches_legacy_context_for_activatable_commands() {
        let (mut core, key) = fixture(false);
        for switched_tab in [false, true] {
            if switched_tab {
                core.new_view_set();
            }
            let composition = core.presentation.as_ref().unwrap();
            let dialog = &composition.dialogs()["launcher"];
            let origin = dialog.context_origin();
            let origin_visible = origin.is_some_and(|origin| {
                composition
                    .mount(origin)
                    .is_some_and(|mount| mount.visible())
            });
            let destination_active = dialog.insertion().is_some_and(|destination| {
                composition.active_surface().is_some_and(|surface| {
                    surface.id() == &destination.surface_instance_id
                        && surface
                            .active_view_set()
                            .is_some_and(|set| set.id == destination.view_set_id)
                })
            });
            let has_surface_origin = origin.is_some_and(|origin| {
                matches!(
                    composition.mount(origin),
                    Some(super::super::presentation_state::MountRef::SurfaceTile { .. })
                )
            }) && origin_visible;
            let mut legacy = core.clone();
            legacy.focused_input = origin.filter(|_| origin_visible).cloned();
            let fallback = core.focused_view_instance_key_without_input_focus();
            let focused = origin.filter(|_| origin_visible).or(fallback.as_ref());
            let exact = |items: Vec<super::super::view_model::RyeOsCommandChoice>| {
                items
                    .into_iter()
                    .filter(|item| {
                        global_or_exact_command(
                            &item.intent,
                            destination_active,
                            has_surface_origin && destination_active,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                exact(super::super::view_model::available_command_items(&legacy)),
                exact(super::super::view_model::available_command_items_with_focus(&core, focused)),
                "captured command choices changed after tab switch: {switched_tab}"
            );
            assert!(composition.mount(&key).is_some());
        }
    }

    #[test]
    fn launcher_record_is_guarded_by_exact_binding_and_captured_tab() {
        let (mut core, key) = fixture(false);
        let producer = test_support::claim_fixture_producer(&mut core);
        test_support::settle_pending_prefix(&mut core, &producer);
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &key, binding).unwrap();
        let record = records
            .iter()
            .find(|record| matches!(record.intent, Some(RyeOsUiIntent::OpenView { .. })))
            .unwrap();
        let id = record.id.clone();
        assert!(activate(&core, &key, &id, &guard).is_some());
        assert!(activate(&core, &key, "unknown", &guard).is_none());
        core.new_view_set();
        test_support::settle_pending_prefix(&mut core, &producer);
        assert!(activate(&core, &key, &id, &guard).is_none());
    }

    #[test]
    fn shared_launcher_keeps_its_frame_but_not_wrong_tab_actions_after_switch() {
        let (mut core, key) = fixture(false);
        let captured = core.active_view_set().unwrap().id;
        let producer = test_support::claim_fixture_producer(&mut core);
        core.new_view_set();
        test_support::settle_pending_prefix(&mut core, &producer);
        assert_ne!(core.active_view_set().unwrap().id, captured);
        let dialog = &core.presentation.as_ref().unwrap().dialogs()["launcher"];
        assert!(dialog.is_open());
        assert_eq!(dialog.insertion().unwrap().view_set_id, captured);
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &key, binding).unwrap();
        assert!(
            records
                .iter()
                .any(|record| record.label == "Captured tab is not active" && !record.enabled)
        );
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.intent, Some(RyeOsUiIntent::OpenView { .. })))
        );
        let RyeOsViewVm::Rows { .. } = project(&core, &key, binding) else {
            panic!("shared launcher should remain an ordinary visible View");
        };
        assert!(records.iter().all(|record| {
            activate(&core, &key, &record.id, &guard)
                .is_none_or(|intent| !matches!(intent, RyeOsUiIntent::OpenView { .. }))
        }));
    }

    #[test]
    fn disabled_launcher_action_remains_discoverable_but_is_explicitly_unavailable() {
        let (core, key) = fixture(false);
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let RyeOsViewVm::Rows { rows, .. } = project(&core, &key, binding) else {
            panic!("launcher must remain an ordinary rows View")
        };
        let close = rows
            .iter()
            .find(|row| row.primary == "Close view set")
            .expect("single-set launcher retains the unavailable close action");
        assert_eq!(close.kind.as_deref(), Some("unavailable"));
        assert_eq!(close.glyph.as_deref(), Some("×"));
        assert!(
            close
                .secondary
                .as_deref()
                .unwrap()
                .starts_with("Unavailable — ")
        );
        assert!(close.intent.is_none());
        assert!(rows.iter().any(|row| row.intent.is_some()));
    }

    #[test]
    fn closed_dialog_cannot_activate_a_delayed_record() {
        let (mut core, key) = fixture(false);
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let (records, guard) = candidates(&core, &key, binding).unwrap();
        let record = records
            .iter()
            .find(|record| record.enabled)
            .unwrap()
            .id
            .clone();
        core.close_dialog(&key);
        assert!(activate(&core, &key, &record, &guard).is_none());
    }

    #[test]
    fn addressed_local_filter_regenerates_rows_without_network_effects() {
        let (mut core, key) = fixture(false);
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        let input = binding.input.as_ref().unwrap();
        assert!(input.is_live_filter());
        let buffer = core
            .input_key_for(key.clone(), "view:ryeos/launcher", input)
            .unwrap();
        let address = core.input_address_for(buffer).unwrap();
        let effects = core.dispatch_addressed_input(
            address.clone(),
            super::super::event::RyeOsInputAction::SetText {
                text: "a query which matches no actual admitted row".into(),
                cursor: 0,
            },
        );
        assert!(effects.is_empty());
        let binding = core
            .binding_for_instance(&key, "view:ryeos/launcher")
            .unwrap();
        assert!(matches!(
            project(&core, &key, binding),
            RyeOsViewVm::Rows { total_rows: 0, .. }
        ));
        assert!(
            core.dispatch_addressed_input(
                address,
                super::super::event::RyeOsInputAction::NavigateSelection { delta: 1 }
            )
            .is_empty()
        );
    }
}
