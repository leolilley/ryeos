//! The decision screen for a seat that cannot be driven.
//!
//! Nothing here infers that the old writer died. The operator retries the
//! exact request (only before a core was installed), or reconciles and
//! inspects the writer and then chooses a replacement explicitly; the node
//! checks the predecessor again when that claim arrives.

use rune::{El, col, el, row, text};
use ryeos_client_runtime::recovery::ReconnectChoice;

use crate::App;
use crate::boot::{Recovery, RecoveryAction};
use crate::theme::{self, Palette};

/// The recovery screen. `confirming` shows the independent presentation's
/// confirmation in place of its first button.
pub fn screen(recovery: &Recovery, confirming: bool) -> El {
    let p = Palette::get();
    let guidance = if recovery.exact_retry {
        "Retry keeps this document's exact request. Reconcile checks retained receipts before \
         showing explicit writer choices."
    } else {
        "This live presentation is fenced. Reconcile retained receipts, inspect the writer, then \
         explicitly choose a replacement. No writer death is inferred."
    };
    let mut actions = row().gap(10.0).flex_wrap();
    if recovery.exact_retry {
        actions = actions.child(action(
            "recovery:retry",
            "Retry exact request",
            RecoveryAction::RetryExact,
        ));
    }
    if recovery.can_inspect {
        actions = actions.child(action(
            "recovery:inspect",
            "Reconcile and inspect writer",
            RecoveryAction::Inspect,
        ));
        if !confirming {
            actions = actions.child(
                button("recovery:independent", "Open independent presentation")
                    .on_press(|app: &mut App| app.local.confirming_independent = true),
            );
        }
    }
    let mut copy = col()
        .gap(14.0)
        .max_w(600.0)
        .role("alert")
        .child(
            text("PRESENTATION NEEDS YOUR DECISION")
                .font(theme::ui(11.0).tracking(1.8))
                .color(p.accent_bright),
        )
        .child(el().w_full().h(2.0).fill(p.line))
        .child(
            text(recovery.message.clone())
                .font(theme::data(12.0))
                .color(p.text)
                .wrap_anywhere()
                .selectable(true)
                .w_full()
                .padding(12.0)
                .border_sides([0.0, 0.0, 0.0, 2.0], p.yellow),
        )
        .child(
            text(guidance)
                .font(theme::ui(13.0))
                .color(p.muted)
                .wrap()
                .w_full(),
        )
        .child(actions);
    if let Some((writer, choices)) = &recovery.observed {
        let mut replacements = row().gap(10.0).flex_wrap();
        for choice in choices {
            let label = match choice {
                ReconnectChoice::Resume { .. } => "Resume relinquished presentation",
                ReconnectChoice::Takeover { .. } => "Take over this presentation",
                ReconnectChoice::Independent => continue,
            };
            replacements = replacements.child(action(
                &format!("recovery:{label}"),
                label,
                RecoveryAction::Choose(choice.clone()),
            ));
        }
        copy = copy
            .child(
                text(format!(
                    "Observed writer: {writer}. Replacement requires an explicit choice; the \
                     node checks the predecessor again."
                ))
                .font(theme::ui(13.0))
                .color(p.text)
                .wrap()
                .w_full(),
            )
            .child(replacements);
    }
    if confirming {
        copy = copy
            .child(
                text(
                    "Open a distinct presentation? This does not resume or take over the \
                     existing writer. Unresolved local writes still must reconcile.",
                )
                .font(theme::ui(13.0))
                .color(p.yellow)
                .wrap()
                .w_full(),
            )
            .child(
                row()
                    .gap(10.0)
                    .child(action(
                        "recovery:independent:confirm",
                        "Open independent presentation",
                        RecoveryAction::Choose(ReconnectChoice::Independent),
                    ))
                    .child(
                        button("recovery:independent:cancel", "Cancel")
                            .on_press(|app: &mut App| app.local.confirming_independent = false),
                    ),
            );
    }
    row()
        .w_full()
        .h_full()
        .center()
        .padding(32.0)
        .fill(p.bg)
        .child(copy)
}

/// The screen while a recovery decision is being carried out.
pub fn working() -> El {
    let p = Palette::get();
    row().w_full().h_full().center().fill(p.bg).child(
        text("Working on your decision…")
            .font(theme::ui(13.0))
            .color(p.yellow)
            .role("status"),
    )
}

fn action(key: &str, label: &str, action: RecoveryAction) -> El {
    button(key, label).on_press(move |app: &mut App| app.recover(action.clone()))
}

fn button(key: &str, label: &str) -> El {
    let p = Palette::get();
    text(label.to_owned())
        .font(theme::ui(13.0))
        .color(p.text)
        .key(key.to_owned())
        .role("button")
        .px(14.0)
        .py(7.0)
        .fill(p.plane_raised)
        .border(1.0, p.line_strong)
}
