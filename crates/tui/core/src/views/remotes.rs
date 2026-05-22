//! Remotes view — alive/cold status, sync state.

use crate::model::AppModel;
use crate::store::RemoteSyncState;
use crate::text_surface::{Style, TextSurface};
use crate::theme;

pub fn build(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header_style = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let alive_style = Style::new().fg(theme::GREEN).bg(theme::BG);
    let dead_style = Style::new().fg(theme::RED).bg(theme::BG);
    let dim_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    // Header
    surface.draw_text(0, 0, "Remotes", header_style);

    // Daemon status
    let daemon = &model.store.daemon;
    let (daemon_status, daemon_style) = match daemon.status {
        crate::store::DaemonStatus::Connected => ("● connected", alive_style),
        crate::store::DaemonStatus::Connecting => ("◌ connecting", dim_style),
        crate::store::DaemonStatus::Disconnected => ("○ disconnected", dead_style),
    };
    if w > 30 {
        surface.draw_text(w.saturating_sub(daemon_status.len() + 1), 0, daemon_status, daemon_style);
    }

    // Remotes
    let mut row = 2;
    for (_, remote) in &model.store.remotes {
        if row >= h {
            break;
        }

        // Status dot
        let (dot, dot_style) = if remote.alive {
            ('●', alive_style)
        } else {
            ('○', dead_style)
        };
        surface.draw_char(1, row, dot, dot_style);

        // Name
        surface.draw_text(3, row, &remote.name, dim_style);

        // URL
        if w > 25 {
            let url_display = if remote.url.len() > w - 25 {
                &remote.url[..w - 25]
            } else {
                &remote.url
            };
            surface.draw_text(20, row, url_display, muted_style);
        }

        // Sync state
        if w > 50 {
            let sync = match remote.sync_state {
                RemoteSyncState::Synced => "synced",
                RemoteSyncState::Ahead => "ahead",
                RemoteSyncState::Behind => "behind",
                RemoteSyncState::Unknown => "—",
            };
            let sync_style = match remote.sync_state {
                RemoteSyncState::Synced => alive_style,
                RemoteSyncState::Ahead => Style::new().fg(theme::ORANGE).bg(theme::BG),
                RemoteSyncState::Behind => Style::new().fg(theme::YELLOW).bg(theme::BG),
                RemoteSyncState::Unknown => muted_style,
            };
            surface.draw_text(w.saturating_sub(sync.len() + 1), row, sync, sync_style);
        }

        row += 1;
    }

    // Empty state
    if model.store.remotes.is_empty() && h > 3 {
        let msg = "No remotes configured";
        let x = w.saturating_sub(msg.len()) / 2;
        surface.draw_text(x, h / 2, msg, muted_style);
    }

    surface
}
