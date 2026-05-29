//! Grid renderer — converts the shared client frame to browser DOM.
//!
//! The browser shell uses the same `Frame` boundary as the terminal client.
//! This renderer keeps the first WASM path deliberately simple: absolute
//! positioned text panes using the shared `TextSurface` data.

use ryeos_client_base::frame::Frame;

/// Render a full frame to HTML.
pub fn render_frame_html(frame: &Frame) -> String {
    let mut html = String::new();
    html.push_str("<div class=\"rye-frame\">");

    for tile in &frame.tiles {
        html.push_str(&surface_block(
            "rye-tile",
            tile.rect,
            &crate::render_dom::generate_html(&tile.cells),
        ));
    }

    html.push_str(&surface_block(
        "rye-status",
        frame.status_bar.rect,
        &crate::render_dom::generate_html(&frame.status_bar.cells),
    ));
    html.push_str(&surface_block(
        "rye-input",
        frame.input.rect,
        &crate::render_dom::generate_html(&frame.input.cells),
    ));

    for overlay in &frame.overlays {
        html.push_str(&surface_block(
            "rye-overlay",
            overlay.rect,
            &crate::render_dom::generate_html(&overlay.cells),
        ));
    }

    html.push_str("</div>");
    html
}

fn surface_block(class_name: &str, rect: ryeos_client_base::layout::Rect, inner: &str) -> String {
    format!(
        "<pre class=\"{}\" style=\"left:{}ch;top:{}em;width:{}ch;height:{}em\">{}</pre>",
        class_name, rect.x, rect.y, rect.w, rect.h, inner
    )
}
