//! Scene primitives and pixel rasterizer.
//!
//! One unified software renderer for both web (WASM Canvas) and terminal
//! (half-block conversion). Renders to a flat RGBA pixel buffer.
//!
//! Pipeline:
//!   3D scene → ScenePrimitive list → rasterize_to_rgba(RGBA buffer)
//!   Web: buffer → putImageData
//!   Terminal: buffer → half-block ▀ characters

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// 2D position in normalized [0, 1] space.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Blend toward background by alpha.
    pub fn blend(&self, bg: &Rgb, alpha: f32) -> Rgb {
        let a = alpha.clamp(0.0, 1.0);
        Rgb::new(
            (bg.r as f32 * (1.0 - a) + self.r as f32 * a) as u8,
            (bg.g as f32 * (1.0 - a) + self.g as f32 * a) as u8,
            (bg.b as f32 * (1.0 - a) + self.b as f32 * a) as u8,
        )
    }

    /// Lerp toward another color by t.
    pub fn lerp(&self, other: &Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        Rgb::new(
            (self.r as f32 * (1.0 - t) + other.r as f32 * t) as u8,
            (self.g as f32 * (1.0 - t) + other.g as f32 * t) as u8,
            (self.b as f32 * (1.0 - t) + other.b as f32 * t) as u8,
        )
    }
}

/// Scene primitive (produced by 3D projection, consumed by rasterizer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScenePrimitive {
    Point {
        pos: Vec2,
        z: f32,
        color: Rgb,
        size: f32,
        opacity: f32,
    },
    Line {
        from: Vec2,
        to: Vec2,
        z: f32,
        color: Rgb,
        thickness: f32,
        opacity: f32,
    },
    Ring {
        center: Vec2,
        radius: f32,
        tilt: f32,
        rotation: f32,
        color: Rgb,
        opacity: f32,
    },
    Polygon {
        vertices: Vec<Vec2>,
        z: f32,
        color: Rgb,
        opacity: f32,
    },
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BG: Rgb = Rgb::new(0x1d, 0x20, 0x21);

/// Fog color (slightly warmer than bg for depth).
const FOG: Rgb = Rgb::new(0x25, 0x28, 0x29);

// ---------------------------------------------------------------------------
// Pixel rasterizer
// ---------------------------------------------------------------------------

/// Render resolution (independent of terminal or browser viewport).
/// 640×360 gives good detail for the orbital scene while staying fast.
pub const RENDER_W: usize = 640;
pub const RENDER_H: usize = 360;

/// Rasterize scene primitives into a flat RGBA pixel buffer.
///
/// `buf` must be `RENDER_W * RENDER_H * 4` bytes.
/// All primitives use normalized [0,1] coordinates — scaled to render resolution.
pub fn rasterize_to_rgba(primitives: &[ScenePrimitive], buf: &mut [u8]) {
    let w = RENDER_W as f32;
    let h = RENDER_H as f32;

    // Clear to background
    for pixel in buf.chunks_exact_mut(4) {
        pixel[0] = BG.r;
        pixel[1] = BG.g;
        pixel[2] = BG.b;
        pixel[3] = 255;
    }

    for prim in primitives {
        match prim {
            ScenePrimitive::Point {
                pos, color, size, opacity, z,
            } => {
                let fogged = apply_fog(color, *z);
                let cx = pos.x * w;
                let cy = pos.y * h;
                let radius = *size * w.max(h) * 0.5;

                // Draw a filled circle with antialiasing at the edge
                let r = radius.ceil() as isize;
                for dy in -r..=r {
                    for dx in -r..=r {
                        let dist = ((dx * dx + dy * dy) as f32).sqrt();
                        if dist <= radius {
                            let edge_aa = (radius - dist).min(1.0);
                            let px = (cx as isize + dx) as usize;
                            let py = (cy as isize + dy) as usize;
                            if px < RENDER_W && py < RENDER_H {
                                blend_pixel(buf, px, py, &fogged, *opacity * edge_aa);
                            }
                        }
                    }
                }
            }
            ScenePrimitive::Line {
                from, to, color, opacity, z, ..
            } => {
                let fogged = apply_fog(color, *z);
                draw_line_wu(
                    buf,
                    from.x * w, from.y * h,
                    to.x * w, to.y * h,
                    &fogged, *opacity,
                );
            }
            ScenePrimitive::Ring {
                center, radius, tilt, rotation, color, opacity,
            } => {
                let cx = center.x * w;
                let cy = center.y * h;
                let rx = radius * w * 0.5;
                let ry = rx * tilt.cos();
                let segments = 64;

                for i in 0..segments {
                    let a1 = (i as f32 / segments as f32) * std::f32::consts::TAU + rotation;
                    let a2 = ((i + 1) as f32 / segments as f32) * std::f32::consts::TAU + rotation;
                    draw_line_wu(
                        buf,
                        cx + a1.cos() * rx, cy + a1.sin() * ry,
                        cx + a2.cos() * rx, cy + a2.sin() * ry,
                        color, *opacity,
                    );
                }
            }
            ScenePrimitive::Polygon {
                vertices, color, opacity, z, ..
            } => {
                if vertices.len() < 2 {
                    continue;
                }
                let fogged = apply_fog(color, *z);
                for i in 0..vertices.len() {
                    let a = &vertices[i];
                    let b = &vertices[(i + 1) % vertices.len()];
                    draw_line_wu(
                        buf,
                        a.x * w, a.y * h,
                        b.x * w, b.y * h,
                        &fogged, *opacity,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Depth fog
// ---------------------------------------------------------------------------

/// Apply distance fog — lerp color toward fog color by z.
/// z is 0.0 (near) to 1.0 (far).
fn apply_fog(color: &Rgb, z: f32) -> Rgb {
    let fog_strength = (z * z * 0.7).clamp(0.0, 1.0);
    color.lerp(&FOG, fog_strength)
}

// ---------------------------------------------------------------------------
// Wu's antialiased line algorithm
// ---------------------------------------------------------------------------

/// Draw an antialiased line using Wu's algorithm.
fn draw_line_wu(
    buf: &mut [u8],
    x0: f32, y0: f32, x1: f32, y1: f32,
    color: &Rgb, opacity: f32,
) {
    let mut x0 = x0;
    let mut y0 = y0;
    let x1 = x1;
    let y1 = y1;

    let steep = (y1 - y0).abs() > (x1 - x0).abs();

    if steep {
        std::mem::swap(&mut x0, &mut y0);
        // We need to also swap x1/y1 for the steep case
    }

    let (x0, y0, x1, y1) = if steep {
        (y0, x0, y1, x1) // already swapped x0↔y0, need to swap x1↔y1 too
    } else {
        (x0, y0, x1, y1)
    };

    // Re-do properly: just implement Wu directly
    let steep = (y1 - y0).abs() > (x1 - x0).abs();
    let (ax, ay, bx, by) = if steep { (y0, x0, y1, x1) } else { (x0, y0, x1, y1) };

    let (ax, ay, bx, by) = if ax > bx { (bx, by, ax, ay) } else { (ax, ay, bx, by) };

    let dx = bx - ax;
    let dy = by - ay;
    let gradient = if dx.abs() < 0.001 { 1.0 } else { dy / dx };

    // First endpoint
    let xend = ax.round();
    let yend = ay + gradient * (xend - ax);
    let xgap = 1.0 - ((ax + 0.5).fract());
    let xpxl1 = xend as isize;
    let ypxl1 = yend.floor() as isize;
    let fpart = yend - yend.floor();

    if steep {
        plot(buf, ypxl1, xpxl1, color, opacity * (1.0 - fpart) * xgap);
        plot(buf, ypxl1 + 1, xpxl1, color, opacity * fpart * xgap);
    } else {
        plot(buf, xpxl1, ypxl1, color, opacity * (1.0 - fpart) * xgap);
        plot(buf, xpxl1, ypxl1 + 1, color, opacity * fpart * xgap);
    }

    let mut intery = yend + gradient;

    // Second endpoint
    let xend = bx.round();
    let yend = by + gradient * (xend - bx);
    let xgap = (bx + 0.5).fract();
    let xpxl2 = xend as isize;
    let ypxl2 = yend.floor() as isize;
    let fpart = yend - yend.floor();

    if steep {
        plot(buf, ypxl2, xpxl2, color, opacity * (1.0 - fpart) * xgap);
        plot(buf, ypxl2 + 1, xpxl2, color, opacity * fpart * xgap);
    } else {
        plot(buf, xpxl2, ypxl2, color, opacity * (1.0 - fpart) * xgap);
        plot(buf, xpxl2, ypxl2 + 1, color, opacity * fpart * xgap);
    }

    // Main loop
    if steep {
        for x in (xpxl1 + 1)..xpxl2 {
            let fy = intery.floor() as isize;
            let fpart = intery - intery.floor();
            plot(buf, fy, x, color, opacity * (1.0 - fpart));
            plot(buf, fy + 1, x, color, opacity * fpart);
            intery += gradient;
        }
    } else {
        for x in (xpxl1 + 1)..xpxl2 {
            let fy = intery.floor() as isize;
            let fpart = intery - intery.floor();
            plot(buf, x, fy, color, opacity * (1.0 - fpart));
            plot(buf, x, fy + 1, color, opacity * fpart);
            intery += gradient;
        }
    }
}

// ---------------------------------------------------------------------------
// Pixel ops
// ---------------------------------------------------------------------------

/// Plot a single pixel with alpha blending.
fn plot(buf: &mut [u8], x: isize, y: isize, color: &Rgb, alpha: f32) {
    if x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as usize, y as usize);
    if x >= RENDER_W || y >= RENDER_H {
        return;
    }
    blend_pixel(buf, x, y, color, alpha);
}

/// Blend a single pixel in the RGBA buffer.
fn blend_pixel(buf: &mut [u8], x: usize, y: usize, color: &Rgb, alpha: f32) {
    let idx = (y * RENDER_W + x) * 4;
    if idx + 3 >= buf.len() {
        return;
    }
    let a = alpha.clamp(0.0, 1.0);
    buf[idx] = (buf[idx] as f32 * (1.0 - a) + color.r as f32 * a) as u8;
    buf[idx + 1] = (buf[idx + 1] as f32 * (1.0 - a) + color.g as f32 * a) as u8;
    buf[idx + 2] = (buf[idx + 2] as f32 * (1.0 - a) + color.b as f32 * a) as u8;
}

// ---------------------------------------------------------------------------
// Half-block conversion (terminal output)
// ---------------------------------------------------------------------------

/// Convert the RGBA pixel buffer to a half-block character string.
///
/// Each terminal cell represents 2 vertical pixels:
/// - Foreground = top pixel color
/// - Background = bottom pixel color
/// - Character = '▀' (upper half block)
///
/// Returns a string of ANSI-escaped characters that paints the full frame.
pub fn rgba_to_halfblock_ansi(buf: &[u8], term_w: usize, term_h: usize) -> String {
    let mut out = String::with_capacity(term_w * term_h * 24);

    for row in 0..term_h {
        let py_top = (row * 2 * RENDER_H) / (term_h * 2);
        let py_bot = ((row * 2 + 1) * RENDER_H) / (term_h * 2);

        for col in 0..term_w {
            let px = (col * RENDER_W) / term_w;

            let top_idx = (py_top * RENDER_W + px) * 4;
            let bot_idx = (py_bot * RENDER_W + px) * 4;

            let (tr, tg, tb) = if top_idx + 3 < buf.len() {
                (buf[top_idx], buf[top_idx + 1], buf[top_idx + 2])
            } else {
                (BG.r, BG.g, BG.b)
            };
            let (br, bg_c, bb) = if bot_idx + 3 < buf.len() {
                (buf[bot_idx], buf[bot_idx + 1], buf[bot_idx + 2])
            } else {
                (BG.r, BG.g, BG.b)
            };

            // fg=38;2;r;g;b  bg=48;2;r;g;b  char=▀
            // Using write! into the String for speed
            use std::fmt::Write;
            write!(out, "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg_c};{bb}m▀").unwrap();
        }
        out.push('\n');
    }
    out.push_str("\x1b[0m");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterize_point() {
        let mut buf = vec![0u8; RENDER_W * RENDER_H * 4];
        let prims = vec![ScenePrimitive::Point {
            pos: Vec2::new(0.5, 0.5),
            z: 0.0,
            color: Rgb::new(0xff, 0x80, 0x00),
            size: 0.02,
            opacity: 1.0,
        }];
        rasterize_to_rgba(&prims, &mut buf);
        // Center pixel should be orange
        let idx = (RENDER_H / 2 * RENDER_W + RENDER_W / 2) * 4;
        assert!(buf[idx] > 200, "red channel should be high, got {}", buf[idx]);
    }

    #[test]
    fn rasterize_line() {
        let mut buf = vec![0u8; RENDER_W * RENDER_H * 4];
        let prims = vec![ScenePrimitive::Line {
            from: Vec2::new(0.0, 0.5),
            to: Vec2::new(1.0, 0.5),
            z: 0.0,
            color: Rgb::new(0xff, 0xff, 0xff),
            thickness: 1.0,
            opacity: 1.0,
        }];
        rasterize_to_rgba(&prims, &mut buf);
        // Middle of the line should be bright
        let idx = (RENDER_H / 2 * RENDER_W + RENDER_W / 2) * 4;
        assert!(buf[idx] > 200, "should be bright, got {}", buf[idx]);
    }

    #[test]
    fn halfblock_produces_output() {
        let mut buf = vec![0u8; RENDER_W * RENDER_H * 4];
        let prims = vec![ScenePrimitive::Point {
            pos: Vec2::new(0.5, 0.5),
            z: 0.0,
            color: Rgb::new(0xff, 0x00, 0x00),
            size: 0.1,
            opacity: 1.0,
        }];
        rasterize_to_rgba(&prims, &mut buf);
        let ansi = rgba_to_halfblock_ansi(&buf, 80, 24);
        assert!(ansi.contains("▀"), "should contain half-block chars");
        assert!(ansi.contains("255;0;0"), "should contain red color");
    }
}
