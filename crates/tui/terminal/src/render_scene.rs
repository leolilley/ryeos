//! Scene renderer — rasterizes ScenePrimitives into a TextSurface
//! using Braille subcells for fine lines and block density for fills.

use ryeos_tui_core::scene::{Rgb, ScenePrimitive};
use ryeos_tui_core::text_surface::{Color, Style, TextSurface};

use crate::braille::BrailleBuffer;

/// Render scene primitives into a TextSurface overlay.
/// The surface should be pre-filled with the background.
pub fn render_scene(primitives: &[ScenePrimitive], surface: &mut TextSurface) {
    let w = surface.width;
    let h = surface.height;
    if w == 0 || h == 0 {
        return;
    }

    // Create a braille buffer at 2x width, 4x height subcell resolution
    let braille_w = w;
    let braille_h = h;
    let mut buf = BrailleBuffer::new(braille_w, braille_h);

    let _bg_rgb = Rgb::new(0x1d, 0x20, 0x21); // BG_DARK

    for prim in primitives {
        match prim {
            ScenePrimitive::Point {
                pos,
                color: _,
                size,
                opacity: _,
                ..
            } => {
                let sub_x = (pos.x * braille_w as f32 * 2.0) as i32;
                let sub_y = (pos.y * braille_h as f32 * 4.0) as i32;
                let radius = (*size * braille_w as f32 * 2.0) as i32;
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        if dx * dx + dy * dy <= radius * radius {
                            let px = sub_x + dx;
                            let py = sub_y + dy;
                            if px >= 0 && py >= 0 {
                                buf.plot(px as usize, py as usize);
                            }
                        }
                    }
                }
                // We'll apply color per-cell below
            }
            ScenePrimitive::Line {
                from,
                to,
                color: _,
                thickness: _,
                opacity: _,
                ..
            } => {
                let x0 = (from.x * braille_w as f32 * 2.0) as i32;
                let y0 = (from.y * braille_h as f32 * 4.0) as i32;
                let x1 = (to.x * braille_w as f32 * 2.0) as i32;
                let y1 = (to.y * braille_h as f32 * 4.0) as i32;
                buf.line(x0, y0, x1, y1);
            }
            ScenePrimitive::Ring {
                center,
                radius,
                tilt,
                rotation,
                color: _,
                opacity: _,
            } => {
                let cx = center.x * braille_w as f32 * 2.0;
                let cy = center.y * braille_h as f32 * 4.0;
                let r = *radius * braille_w as f32 * 2.0;
                let steps = (r * 4.0) as usize;
                let steps = steps.clamp(20, 200);

                for i in 0..steps {
                    let angle = *rotation + (i as f32 / steps as f32) * std::f32::consts::TAU;
                    let rx = r;
                    let ry = r * tilt.cos();
                    let px = (cx + angle.cos() * rx) as i32;
                    let py = (cy + angle.sin() * ry) as i32;
                    if px >= 0 && py >= 0 {
                        buf.plot(px as usize, py as usize);
                    }
                }
            }
            ScenePrimitive::Polygon { .. } => {
                // V1: skip polygons, they're for the web renderer
            }
        }
    }

    // Stamp braille buffer onto the text surface with color
    // Use the average color from all primitives for now (V1 simplification)
    let substrate_color = Color::Rgb(0xfe, 0x80, 0x19); // orange shard color

    for cy in 0..braille_h {
        for cx in 0..braille_w {
            let ch = buf.get_char(cx, cy);
            if ch != '⠀' {
                let style = Style::new()
                    .fg(substrate_color)
                    .bg(Color::Rgb(0x1d, 0x20, 0x21));
                surface.draw_char(cx, cy, ch, style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tui_core::scene::Vec2;

    #[test]
    fn render_scene_ignores_unsupported_without_panic() {
        let mut surface = TextSurface::new(20, 10);
        surface.fill(Style::new().bg(Color::Rgb(0x1d, 0x20, 0x21)));

        let prims = vec![ScenePrimitive::Polygon {
            vertices: vec![Vec2::new(0.0, 0.0), Vec2::new(1.0, 0.0)],
            z: 0.0,
            color: Rgb::new(255, 255, 255),
            opacity: 1.0,
        }];

        render_scene(&prims, &mut surface);
        // Should not panic, polygon is skipped
    }

    #[test]
    fn render_scene_draws_point() {
        let mut surface = TextSurface::new(40, 20);
        surface.fill(Style::new().bg(Color::Rgb(0x1d, 0x20, 0x21)));

        let prims = vec![ScenePrimitive::Point {
            pos: Vec2::new(0.5, 0.5),
            z: 1.0,
            color: Rgb::new(0xfe, 0x80, 0x19),
            size: 0.02,
            opacity: 1.0,
        }];

        render_scene(&prims, &mut surface);
        // Should have drawn something near center
        let center_cell = surface.get(20, 10);
        assert_ne!(center_cell.rune, ' ');
    }
}
