//! The interface symbols, as vector icons.
//!
//! On the web Rune has only its embedded faces (Inter and IBM Plex Mono) and
//! no system fallback, so a symbol neither face carries (disclosure
//! triangles, the vertical ellipsis, the pencil) would draw as a missing
//! glyph. These draw in their element's colour at any size.

use rune::{Color, El, Icon};

macro_rules! line_icon {
    ($name:ident, $id:literal, $shape:literal) => {
        pub static $name: Icon = Icon {
            name: $id,
            view: 16.0,
            body: concat!(
                r#"<g fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">"#,
                $shape,
                "</g>"
            ),
        };
    };
}

macro_rules! solid_icon {
    ($name:ident, $id:literal, $shape:literal) => {
        pub static $name: Icon = Icon {
            name: $id,
            view: 16.0,
            body: concat!(r#"<g fill="currentColor">"#, $shape, "</g>"),
        };
    };
}

// Disclosure: collapsed points right, open points down.
solid_icon!(
    COLLAPSED,
    "ryeos-collapsed",
    r#"<path d="M6 4.5 L11 8 L6 11.5 Z"/>"#
);
solid_icon!(
    EXPANDED,
    "ryeos-expanded",
    r#"<path d="M4.5 6 L11.5 6 L8 11 Z"/>"#
);
solid_icon!(
    MORE,
    "ryeos-more",
    r#"<circle cx="8" cy="3.5" r="1.25"/><circle cx="8" cy="8" r="1.25"/><circle cx="8" cy="12.5" r="1.25"/>"#
);
line_icon!(
    PENCIL,
    "ryeos-pencil",
    r#"<path d="M10.5 3 L13 5.5 L6 12.5 L3 13 L3.5 10 Z"/><path d="M9 4.5 L11.5 7"/>"#
);
line_icon!(
    DUPLICATE,
    "ryeos-duplicate",
    r#"<rect x="5.5" y="5.5" width="7.5" height="7.5" rx="1"/><path d="M3 10.5 V4 a1 1 0 0 1 1 -1 H10.5"/>"#
);
line_icon!(
    CLOSE,
    "ryeos-close",
    r#"<path d="M4 4 L12 12 M12 4 L4 12"/>"#
);
line_icon!(PLUS, "ryeos-plus", r#"<path d="M8 3 V13 M3 8 H13"/>"#);
solid_icon!(
    PREVIOUS,
    "ryeos-previous",
    r#"<path d="M11 3.5 L4.5 8 L11 12.5 Z"/>"#
);
solid_icon!(
    NEXT,
    "ryeos-next",
    r#"<path d="M5 3.5 L11.5 8 L5 12.5 Z"/>"#
);
solid_icon!(
    SQUARE,
    "ryeos-square",
    r#"<rect x="4" y="4" width="8" height="8"/>"#
);
solid_icon!(DOT, "ryeos-dot", r#"<circle cx="8" cy="8" r="3.5"/>"#);
line_icon!(RING, "ryeos-ring", r#"<circle cx="8" cy="8" r="3.25"/>"#);

/// `icon` at `size` points in `color`.
pub fn glyph(icon: &'static Icon, size: f32, color: Color) -> El {
    rune::icon(icon, size).color(color).a11y_hidden()
}
