//! The decision screen for a seat that cannot be driven.
//!
//! Nothing here infers that the old writer died. The operator retries the
//! exact request (only before a core was installed), or reconciles and
//! inspects the writer and then chooses a replacement explicitly; the node
//! checks the predecessor again when that claim arrives.

use rune::{El, el, row, text};
use ryeos_client_runtime::recovery::ReconnectChoice;

use crate::App;
use crate::boot::{Recovery, RecoveryAction};
use crate::theme::{self, Palette};

/// The recovery screen. `confirming` shows the independent presentation's
/// confirmation in place of its first button.
pub fn screen(recovery: &Recovery, confirming: bool) -> El {
    let p = Palette::get();
    let guidance = match (&recovery.observed, recovery.exact_retry) {
        (Some(_), _) => {
            "It is open in another tab or window, or in one that closed without letting go. \
             Taking over moves it here; the node checks the writer again first."
        }
        (None, true) => {
            "Retry sends the same request again. Inspect reads the current writer and checks \
             retained receipts before offering choices."
        }
        (None, false) => {
            "Changes are paused. Inspect the current writer, then choose how to continue; \
             nothing is taken over without your choice."
        }
    };
    let mut actions = row().gap(8.0).flex_wrap();
    if recovery.exact_retry {
        actions = actions.child(action(
            "recovery:retry",
            "Retry",
            RecoveryAction::RetryExact,
        ));
    }
    if recovery.can_inspect {
        actions = actions.child(action(
            "recovery:inspect",
            "Inspect writer",
            RecoveryAction::Inspect,
        ));
        if !confirming {
            actions = actions.child(
                button("recovery:independent", "Open separately")
                    .on_press(|app: &mut App| app.local.confirming_independent = true),
            );
        }
    }
    let mut copy =
        crate::shell::frame_column("This presentation needs a decision", p.text, p.yellow)
            .role("alert")
            .child(
                text(recovery.message.clone())
                    .font(theme::data(12.0))
                    .color(p.yellow)
                    .wrap_anywhere()
                    .selectable(true)
                    .w_full(),
            )
            .child(
                text(guidance)
                    .font(theme::ui(12.0))
                    .color(p.muted)
                    .wrap()
                    .w_full(),
            )
            .child(actions);
    if let Some((writer, choices)) = &recovery.observed {
        let mut replacements = row().gap(8.0).flex_wrap();
        for choice in choices {
            let (key, label) = match choice {
                ReconnectChoice::Resume { .. } => ("resume", "Resume"),
                ReconnectChoice::Takeover { .. } => ("takeover", "Take over"),
                ReconnectChoice::Independent => continue,
            };
            replacements = replacements.child(action(
                &format!("recovery:{key}"),
                label,
                RecoveryAction::Choose(choice.clone()),
            ));
        }
        copy = copy
            .child(el().w_full().h(1.0).fill(p.line))
            .child(
                text(format!("WRITER · {}", writer.to_uppercase()))
                    .font(theme::data(10.0).tracking(1.6))
                    .color(p.muted),
            )
            .child(replacements);
    }
    if confirming {
        copy = copy
            .child(el().w_full().h(1.0).fill(p.line))
            .child(
                text(
                    "Open a separate presentation? The current writer keeps this one; \
                     unresolved local writes still reconcile first.",
                )
                .font(theme::ui(12.0))
                .color(p.text)
                .wrap()
                .w_full(),
            )
            .child(
                row()
                    .gap(8.0)
                    .child(action(
                        "recovery:independent:confirm",
                        "Open separately",
                        RecoveryAction::Choose(ReconnectChoice::Independent),
                    ))
                    .child(
                        button("recovery:independent:cancel", "Cancel")
                            .on_press(|app: &mut App| app.local.confirming_independent = false),
                    ),
            );
    }
    crate::shell::centered(copy.max_w(520.0))
}

/// The screen while a recovery decision is being carried out.
pub fn working() -> El {
    let p = Palette::get();
    crate::shell::quiet_frame(
        "Working on your decision",
        "The writer choice is being applied.",
        p.accent,
    )
}

fn action(key: &str, label: &str, action: RecoveryAction) -> El {
    button(key, label).on_press(move |app: &mut App| app.recover(action.clone()))
}

/// A line button: a hairline outline that fills on hover.
fn button(key: &str, label: &str) -> El {
    let p = Palette::get();
    text(label.to_owned())
        .font(theme::ui(12.0))
        .color(p.text)
        .key(key.to_owned())
        .role("button")
        .px(14.0)
        .py(6.0)
        .border(1.0, p.line_strong)
        .hover(p.plane_raised)
}
