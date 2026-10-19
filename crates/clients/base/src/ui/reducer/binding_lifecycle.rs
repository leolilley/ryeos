use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::model::RyeOsCore;
use super::view_model::{RyeOsOverlayChoice, RyeOsTone};

impl RyeOsCore {
    pub(crate) fn releasable_binding_attachment_items(&self) -> Vec<RyeOsOverlayChoice> {
        self.binding_attachments
            .values()
            .filter(|attachment| {
                self.binding_attachment_is_locally_unused(
                    &attachment.descriptor.binding_attachment_id,
                    attachment.descriptor.binding_generation,
                    &attachment.descriptor.binding_digest,
                )
            })
            .map(|attachment| {
                let descriptor = &attachment.descriptor;
                let subject = descriptor
                    .project_path
                    .as_deref()
                    .unwrap_or(&descriptor.surface_ref);
                RyeOsOverlayChoice {
                    label: format!("Release project context · {subject}"),
                    hint: "session-wide release; affects other browser tabs; does not stop work"
                        .into(),
                    intent: super::event::RyeOsUiIntent::ReleaseBindingAttachment {
                        binding_attachment_id: descriptor.binding_attachment_id.clone(),
                        binding_generation: descriptor.binding_generation,
                        binding_digest: descriptor.binding_digest.clone(),
                    },
                    secondary_intent: None,
                    enabled: true,
                }
            })
            .collect()
    }

    pub(crate) fn binding_attachment_is_locally_unused(
        &self,
        attachment_id: &str,
        generation: u64,
        digest: &str,
    ) -> bool {
        if attachment_id == self.surface_attachment_id {
            return false;
        }
        let Some(attachment) = self.binding_attachment(attachment_id) else {
            return false;
        };
        if attachment.binding_generation != generation || attachment.binding_digest != digest {
            return false;
        }
        !self
            .instance_binding_attachments
            .values()
            .any(|id| id == attachment_id)
            && !self
                .view_set_insertion_attachments
                .values()
                .any(|id| id == attachment_id)
            && !self.view_sets.iter().any(|view_set| {
                view_set
                    .lens_stack
                    .iter()
                    .any(|frame| frame.binding_attachment_id == attachment_id)
            })
            && !self.pending_effects.values().any(|effect| match effect {
                RyeOsEffectKind::FetchSource { request, .. }
                | RyeOsEffectKind::InvokeBinding { request, .. } => {
                    request.binding_attachment_id == attachment_id
                }
                RyeOsEffectKind::ReleaseBindingAttachment {
                    binding_attachment_id,
                    ..
                } => binding_attachment_id == attachment_id,
                _ => false,
            })
    }

    pub(crate) fn release_binding_attachment(
        &mut self,
        binding_attachment_id: String,
        binding_generation: u64,
        binding_digest: String,
    ) -> Vec<RyeOsEffect> {
        if !self.binding_attachment_is_locally_unused(
            &binding_attachment_id,
            binding_generation,
            &binding_digest,
        ) {
            self.notice(
                "That project context is still in use or has changed; it was not released.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        vec![self.emit(RyeOsEffectKind::ReleaseBindingAttachment {
            binding_attachment_id,
            binding_generation,
            binding_digest,
        })]
    }

    pub(crate) fn apply_released_binding_attachment(
        &mut self,
        binding_attachment_id: &str,
        binding_generation: u64,
        binding_digest: &str,
        data: serde_json::Value,
    ) {
        let response = data.get("attachment");
        let response_matches = data
            .get("detached")
            .and_then(serde_json::Value::as_bool)
            .is_some()
            && response
                .and_then(|value| value.get("binding_attachment_id"))
                .and_then(serde_json::Value::as_str)
                == Some(binding_attachment_id)
            && response
                .and_then(|value| value.get("binding_generation"))
                .and_then(serde_json::Value::as_u64)
                == Some(binding_generation)
            && response
                .and_then(|value| value.get("binding_digest"))
                .and_then(serde_json::Value::as_str)
                == Some(binding_digest);
        if !response_matches
            || !self.binding_attachment_is_locally_unused(
                binding_attachment_id,
                binding_generation,
                binding_digest,
            )
        {
            self.notice(
                "The project-context release response was stale or invalid; local state was retained.",
                RyeOsTone::Danger,
            );
            return;
        }
        self.binding_attachments.remove(binding_attachment_id);
        if let Some(session) = self.data.session.as_mut() {
            session.binding_attachments.retain(|attachment| {
                attachment.binding_attachment_id != binding_attachment_id
                    || attachment.binding_generation != binding_generation
                    || attachment.binding_digest != binding_digest
            });
        }
        self.notice(
            "Project context released from this session; running work was not stopped.",
            RyeOsTone::Good,
        );
        self.bump_generation();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;

    fn core_with_unused_attachment() -> RyeOsCore {
        let surface = fixture_attachment(
            "surface",
            1,
            &"11".repeat(32),
            None,
            serde_json::json!({"name":"surface"}),
        );
        let project = fixture_attachment(
            "project",
            7,
            &"77".repeat(32),
            Some("/projects/seven"),
            serde_json::json!({"name":"project"}),
        );
        RyeOsCore::new(
            session_with_attachments("surface", vec![surface, project]),
            BrowserViewport::default(),
            0,
        )
    }

    #[test]
    fn release_is_offered_only_for_an_unused_non_surface_attachment() {
        let mut core = core_with_unused_attachment();
        let items = core.releasable_binding_attachment_items();
        assert_eq!(items.len(), 1);
        assert!(items[0].label.contains("/projects/seven"));

        core.view_set_insertion_attachments
            .insert(core.view_sets[0].id, "project".into());
        assert!(core.releasable_binding_attachment_items().is_empty());
    }

    #[test]
    fn exact_release_confirmation_removes_only_the_matching_attachment() {
        let mut core = core_with_unused_attachment();
        let effects = core.release_binding_attachment("project".into(), 7, "77".repeat(32));
        assert_eq!(effects.len(), 1);
        assert!(core.releasable_binding_attachment_items().is_empty());

        core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
            id: effects[0].id,
            ok: true,
            kind: crate::ui::effect::RyeOsEffectResultKind::BrowserOnly,
            data: Some(serde_json::json!({
                "detached": true,
                "attachment": {
                    "binding_attachment_id": "project",
                    "binding_generation": 7,
                    "binding_digest": "77".repeat(32),
                }
            })),
            error: None,
        });

        assert!(core.binding_attachments.contains_key("surface"));
        assert!(!core.binding_attachments.contains_key("project"));
        let session = core.data.session.as_ref().expect("session retained");
        assert_eq!(session.binding_attachments.len(), 1);
        assert_eq!(
            session.binding_attachments[0].binding_attachment_id,
            "surface"
        );
    }

    #[test]
    fn failed_release_retains_attachment_and_allows_explicit_retry() {
        let mut core = core_with_unused_attachment();
        let effect = core
            .release_binding_attachment("project".into(), 7, "77".repeat(32))
            .remove(0);
        core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
            id: effect.id,
            ok: false,
            kind: crate::ui::effect::RyeOsEffectResultKind::BrowserOnly,
            data: None,
            error: Some(crate::ui::effect::RyeOsUiError::definite(
                "ui_binding_stale",
                "stale attachment",
            )),
        });

        assert!(core.binding_attachments.contains_key("project"));
        assert_eq!(core.releasable_binding_attachment_items().len(), 1);
    }
}
