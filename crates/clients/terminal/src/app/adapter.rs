//! The effects only a host can perform. The terminal implements none of them:
//! each is a definite failure naming the missing capability, never a success
//! the core would record as done.

use std::rc::Rc;

use ryeos_client_base::ui::{RyeOsEffect, RyeOsEffectKind};
use ryeos_client_runtime::RuntimeError;
use ryeos_client_runtime::effects::{AdapterFuture, AdapterHandler};

fn missing_capability(kind: &RyeOsEffectKind) -> RuntimeError {
    let capability = match kind {
        RyeOsEffectKind::OpenNewPresentationDocument { .. }
        | RyeOsEffectKind::ShowPreparedPresentationDocument { .. } => {
            "independent document hosting"
        }
        RyeOsEffectKind::SetLocationHash { .. } => "location hash navigation",
        RyeOsEffectKind::CopyToClipboard { .. } => "clipboard copying",
        RyeOsEffectKind::OpenUrl { .. } => "external URL opening",
        _ => "this effect",
    };
    RuntimeError::BindingRequest(format!("Native terminal does not implement {capability}."))
}

/// The terminal's handler for host-owned effects.
pub fn terminal_adapter() -> AdapterHandler {
    Rc::new(|effect: RyeOsEffect| -> AdapterFuture {
        Box::pin(async move { Err(missing_capability(&effect.kind)) })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::RyeOsEffectResultKind;
    use ryeos_client_runtime::effects::failure_result;

    #[test]
    fn unsupported_native_capabilities_are_definite_failures_not_success() {
        let handler = terminal_adapter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        for kind in [
            RyeOsEffectKind::OpenNewPresentationDocument {
                expected_session_id: "fixture-session".into(),
                root_binding: ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                    binding_attachment_id: "fixture-root".into(),
                    binding_generation: 1,
                    binding_digest: "ab".repeat(32),
                },
            },
            RyeOsEffectKind::ShowPreparedPresentationDocument {
                presentation_id: "11111111-1111-4111-8111-111111111111".into(),
            },
            RyeOsEffectKind::SetLocationHash {
                hash: "#fixture".into(),
            },
            RyeOsEffectKind::CopyToClipboard {
                text: "fixture".into(),
            },
            RyeOsEffectKind::OpenUrl {
                url: "https://example.invalid/".into(),
            },
        ] {
            let effect = RyeOsEffect { id: 42, kind };
            let outcome = runtime.block_on(handler(effect.clone()));
            let error = outcome.expect_err("the terminal implements no host effect");
            let result = failure_result(&effect, &error);
            assert_eq!(result.id, effect.id);
            assert_eq!(result.kind, RyeOsEffectResultKind::BrowserOnly);
            assert!(!result.ok);
            assert!(result.data.is_none());
            let error = result.error.unwrap();
            assert_eq!(error.code, "platform_effect_failed");
            assert!(error.message.contains("does not implement"));
        }
    }
}
