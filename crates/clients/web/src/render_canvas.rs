//! Canvas 2D renderer — converts scene primitives to Canvas API calls.
//!
//! Generates JavaScript code for drawing scene primitives on an HTML5 Canvas.
//! Used by the web crate for animated backgrounds.

use ryeos_tui_core::scene::{Rgb, ScenePrimitive};

/// Generate Canvas 2D API JavaScript for a list of scene primitives.
/// Returns a JS string that draws all primitives on the given canvas context.
pub fn render_to_canvas_js(primitives: &[ScenePrimitive], canvas_id: &str) -> String {
    let mut js = String::with_capacity(1024);
    js.push_str(&format!(
        "const ctx = document.getElementById('{}').getContext('2d');\n",
        canvas_id
    ));
    js.push_str("const W = ctx.canvas.width;\nconst H = ctx.canvas.height;\n");
    js.push_str("ctx.clearRect(0, 0, W, H);\n");

    for prim in primitives {
        render_primitive_js(&mut js, prim);
    }

    js
}

fn render_primitive_js(js: &mut String, prim: &ScenePrimitive) {
    match prim {
        ScenePrimitive::Point {
            pos,
            z: _,
            color,
            size,
            opacity,
        } => {
            let px = pos.x;
            let py = pos.y;
            js.push_str(&format!(
                "ctx.fillStyle = '{}';\nctx.globalAlpha = {:.2};\nctx.beginPath();\nctx.arc({}*W, {}*H, {}, 0, Math.PI * 2);\nctx.fill();\n",
                rgb_to_js(color),
                opacity,
                px, py, size * 50.0
            ));
        }
        ScenePrimitive::Line {
            from,
            to,
            z: _,
            color,
            thickness,
            opacity,
        } => {
            js.push_str(&format!(
                "ctx.strokeStyle = '{}';\nctx.globalAlpha = {:.2};\nctx.lineWidth = {};\nctx.beginPath();\nctx.moveTo({}*W, {}*H);\nctx.lineTo({}*W, {}*H);\nctx.stroke();\n",
                rgb_to_js(color),
                opacity,
                thickness * 2.0,
                from.x, from.y, to.x, to.y
            ));
        }
        ScenePrimitive::Ring {
            center,
            radius,
            tilt,
            rotation,
            color,
            opacity,
        } => {
            let rx = radius * 0.5;
            let ry = radius * 0.5 * tilt.cos();
            js.push_str(&format!(
                "ctx.strokeStyle = '{}';\nctx.globalAlpha = {:.2};\nctx.lineWidth = 1.5;\nctx.beginPath();\nctx.ellipse({}*W, {}*H, {}*W, {}*H, {}, 0, Math.PI * 2);\nctx.stroke();\n",
                rgb_to_js(color),
                opacity,
                center.x, center.y, rx, ry, rotation
            ));
        }
        ScenePrimitive::Polygon {
            vertices,
            z: _,
            color,
            opacity,
        } => {
            if vertices.len() < 2 {
                return;
            }
            // Stroke the polygon outline (used for orbital ring outlines)
            js.push_str(&format!(
                "ctx.strokeStyle = '{}';\nctx.globalAlpha = {:.2};\nctx.lineWidth = 1.5;\nctx.beginPath();\nctx.moveTo({}*W, {}*H);\n",
                rgb_to_js(color),
                opacity,
                vertices[0].x, vertices[0].y
            ));
            for v in &vertices[1..] {
                js.push_str(&format!("ctx.lineTo({}*W, {}*H);\n", v.x, v.y));
            }
            js.push_str("ctx.closePath();\nctx.stroke();\n");
        }
    }
}

fn rgb_to_js(color: &Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tui_core::scene::{Rgb, ScenePrimitive, Vec2};

    #[test]
    fn canvas_js_point() {
        let prim = ScenePrimitive::Point {
            pos: Vec2::new(0.5, 0.5),
            z: 0.0,
            color: Rgb::new(0xfe, 0x80, 0x19),
            size: 0.1,
            opacity: 0.5,
        };
        let js = render_to_canvas_js(&[prim], "bg-canvas");
        assert!(js.contains("#fe8019"));
        assert!(js.contains("0.5*W"));
        assert!(js.contains("0.5*H"));
    }

    #[test]
    fn canvas_js_line() {
        let prim = ScenePrimitive::Line {
            from: Vec2::new(0.0, 0.0),
            to: Vec2::new(1.0, 1.0),
            z: 0.0,
            color: Rgb::new(0x83, 0xa5, 0x98),
            thickness: 1.0,
            opacity: 0.3,
        };
        let js = render_to_canvas_js(&[prim], "bg-canvas");
        assert!(js.contains("#83a598"));
        assert!(js.contains("moveTo"));
        assert!(js.contains("lineTo"));
    }

    #[test]
    fn canvas_js_ring() {
        let prim = ScenePrimitive::Ring {
            center: Vec2::new(0.5, 0.5),
            radius: 0.3,
            tilt: 0.5,
            rotation: 1.0,
            color: Rgb::new(0xb8, 0xbb, 0x26),
            opacity: 0.4,
        };
        let js = render_to_canvas_js(&[prim], "bg-canvas");
        assert!(js.contains("#b8bb26"));
        assert!(js.contains("ellipse"));
    }

    #[test]
    fn canvas_js_polygon() {
        let prim = ScenePrimitive::Polygon {
            vertices: vec![
                Vec2::new(0.0, 0.0),
                Vec2::new(1.0, 0.0),
                Vec2::new(0.5, 1.0),
            ],
            z: 0.0,
            color: Rgb::new(0xcc, 0x24, 0x1d),
            opacity: 0.6,
        };
        let js = render_to_canvas_js(&[prim], "bg-canvas");
        assert!(js.contains("#cc241d"));
        assert!(js.contains("closePath"));
    }
}
