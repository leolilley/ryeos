//! Trust view — signature verification status across spaces.

use crate::model::AppModel;
use crate::store::TrustSeverity;
use crate::text_surface::Style;
use crate::text_surface::TextSurface;
use crate::theme;

pub fn build(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header_style = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let ok_style = Style::new().fg(theme::GREEN).bg(theme::BG);
    let warn_style = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let err_style = Style::new().fg(theme::RED).bg(theme::BG);
    let dim_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    // Header
    surface.draw_text(0, 0, "Trust", header_style);

    // Identity section
    let mut row = 2;
    if let Some(identity) = &model.store.identity {
        surface.draw_text(
            1,
            row,
            "Identity",
            Style::new().fg(theme::ACCENT).bg(theme::BG).bold(),
        );
        row += 1;

        let fp = format!("  Fingerprint: {}", identity.fingerprint);
        surface.draw_text(1, row, &fp, dim_style);
        row += 1;

        let key_status = if identity.has_signing_key {
            ("  Signing key: present", ok_style)
        } else {
            ("  Signing key: missing", err_style)
        };
        surface.draw_text(1, row, key_status.0, key_status.1);
        row += 2;
    }

    // Trust alerts
    if !model.store.trust_alerts.is_empty() {
        surface.draw_text(
            1,
            row,
            "Alerts",
            Style::new().fg(theme::ACCENT).bg(theme::BG).bold(),
        );
        row += 1;

        for alert in &model.store.trust_alerts {
            if row >= h {
                break;
            }
            let (icon, style) = match alert.severity {
                TrustSeverity::Info => ('◎', dim_style),
                TrustSeverity::Warning => ('⚠', warn_style),
                TrustSeverity::Error => ('✗', err_style),
            };
            surface.draw_char(2, row, icon, style);
            let msg = if alert.message.len() > w.saturating_sub(5) {
                &alert.message[..w.saturating_sub(5)]
            } else {
                &alert.message
            };
            surface.draw_text(4, row, msg, style);
            row += 1;
        }
        row += 1;
    }

    // Item signature summary
    let total_items = model.store.items.len();
    let signed_count = model.store.items.values().filter(|i| i.signed).count();
    let unsigned_count = total_items - signed_count;

    if row < h {
        surface.draw_text(
            1,
            row,
            "Items",
            Style::new().fg(theme::ACCENT).bg(theme::BG).bold(),
        );
        row += 1;

        let summary = format!("  ✓ {} signed · ○ {} unsigned", signed_count, unsigned_count);
        surface.draw_text(1, row, &summary, dim_style);
        row += 1;

        // Per-space breakdown
        if row < h {
            let spaces = format!(
                "  project: {} items · user: - · system: -",
                total_items
            );
            surface.draw_text(1, row, &spaces, muted_style);
        }
    }

    // Empty state
    if total_items == 0 && model.store.trust_alerts.is_empty() {
        let msg = "No trust data yet";
        let x = w.saturating_sub(msg.len()) / 2;
        let y = h / 2;
        if y < h {
            surface.draw_text(x, y, msg, muted_style);
        }
    }

    surface
}
