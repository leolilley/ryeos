//! Terminal renderer — rasterizes ScenePrimitives into a braille framebuffer.
//!
//! Uses ColoredBrailleBuffer for 2×4 sub-cell resolution.
//! Wu antialiased lines with sub-cell positioning for smooth curves.

use ryeos_tui_core::scene::{Rgb, ScenePrimitive};

use crate::braille::ColoredBrailleBuffer;

/// Render scene primitives into a braille buffer.
/// `term_w` and `term_h` are terminal cell dimensions.
/// The braille pixel space is term_w*2 × term_h*4.
pub fn render_to_braille(primitives: &[ScenePrimitive], buf: &mut ColoredBrailleBuffer) {
    buf.clear();

    let bw = buf.pixel_w() as f32;
    let bh = buf.pixel_h() as f32;

    for prim in primitives {
        match prim {
            ScenePrimitive::Point {
                pos,
                color,
                size,
                opacity,
                ..
            } => {
                let px = pos.x * bw;
                let py = pos.y * bh;
                let radius = *size * bw.max(bh) * 0.5;

                // Draw a filled circle
                let r = radius.ceil() as isize;
                for dy in -r..=r {
                    for dx in -r..=r {
                        let dist = ((dx * dx + dy * dy) as f32).sqrt();
                        if dist <= radius {
                            let edge = (radius - dist).min(1.0);
                            let x = (px as isize + dx) as usize;
                            let y = (py as isize + dy) as usize;
                            if x < buf.pixel_w() && y < buf.pixel_h() {
                                buf.plot(x, y, *color, *opacity * edge);
                            }
                        }
                    }
                }
            }

            ScenePrimitive::Line {
                from,
                to,
                color,
                opacity,
                z,
                ..
            } => {
                let fogged = apply_fog(color, *z);
                buf.wu_line(
                    from.x * bw,
                    from.y * bh,
                    to.x * bw,
                    to.y * bh,
                    fogged,
                    *opacity,
                );
            }

            ScenePrimitive::Ring {
                center,
                radius,
                tilt,
                rotation,
                color,
                opacity,
            } => {
                let cx = center.x * bw;
                let cy = center.y * bh;
                let rx = radius * bw * 0.5;
                let ry = rx * tilt.cos();
                let segments = 48;

                for i in 0..segments {
                    let a1 = (i as f32 / segments as f32) * std::f32::consts::TAU + rotation;
                    let a2 = ((i + 1) as f32 / segments as f32) * std::f32::consts::TAU + rotation;
                    buf.wu_line(
                        cx + a1.cos() * rx,
                        cy + a1.sin() * ry,
                        cx + a2.cos() * rx,
                        cy + a2.sin() * ry,
                        *color,
                        *opacity,
                    );
                }
            }

            ScenePrimitive::Polygon {
                vertices,
                color,
                opacity,
                z,
            } => {
                if vertices.len() < 2 {
                    continue;
                }
                let fogged = apply_fog(color, *z);
                for i in 0..vertices.len() {
                    let a = &vertices[i];
                    let b = &vertices[(i + 1) % vertices.len()];
                    buf.wu_line(a.x * bw, a.y * bh, b.x * bw, b.y * bh, fogged, *opacity);
                }
            }
        }
    }
}

/// Depth fog — lerp toward dark bg by z.
fn apply_fog(color: &Rgb, z: f32) -> Rgb {
    let fog = Rgb::new(0x25, 0x28, 0x29);
    let t = (z * z * 0.7).clamp(0.0, 1.0);
    Rgb::new(
        (color.r as f32 * (1.0 - t) + fog.r as f32 * t) as u8,
        (color.g as f32 * (1.0 - t) + fog.g as f32 * t) as u8,
        (color.b as f32 * (1.0 - t) + fog.b as f32 * t) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tui_core::scene::Vec2;

    #[test]
    fn render_line_sets_dots() {
        let mut buf = ColoredBrailleBuffer::new(40, 12);
        let prims = vec![ScenePrimitive::Line {
            from: Vec2::new(0.0, 0.5),
            to: Vec2::new(1.0, 0.5),
            z: 0.0,
            color: Rgb::new(0xff, 0x80, 0x00),
            thickness: 1.0,
            opacity: 1.0,
        }];
        render_to_braille(&prims, &mut buf);
        assert!(
            buf.to_ansi().contains("⠠")
                || buf.to_ansi().contains("⡀")
                || !buf.to_ansi().contains("⠀"),
            "line should set some dots"
        );
    }

    #[test]
    fn render_empty_no_crash() {
        let mut buf = ColoredBrailleBuffer::new(10, 5);
        render_to_braille(&[], &mut buf);
    }
}
