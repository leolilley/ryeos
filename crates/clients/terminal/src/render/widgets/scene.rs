//! The generic scene renderer: ONE renderer for every `widget: scene`
//! (the backdrop, the atlas, any future scene). It draws whatever objects
//! a `StudioSceneModel` declares — it knows nothing about "the shard" or
//! "the starfield". The background is content; this is the closed
//! primitive that draws it.
//!
//! Contract (spec §"The scene → cell contract"):
//! - orthographic-project each object's `[x, y]` into the target rect
//!   (camera gives pan/zoom; +y is up, so rows flip);
//! - glyph per object: a `particle` → a dot sized by `scale` (`·`/`•`/`●`);
//!   a `text` → its `label`;
//! - colour by `tone` via the theme (never invent colour);
//! - particles TWINKLE: glyph size and opacity-dim vary by a function of
//!   the scene's `generation`, with a per-object phase from the object id
//!   so they don't pulse in unison. The renderer steps by `generation`;
//!   the scene only declares particles — the motion is generic.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::scene_model::{
    StudioSceneModel, StudioSceneObjectKind, StudioSceneObjectVm,
};
use ryeos_client_base::studio::view_model::StudioTone;
use ryeos_client_base::text_surface::{Color, Style, TextSurface};

use super::super::text::display_width;
use super::super::theme::{ACCENT, BG, DANGER, FG_SOFT, GOOD, MUTED, WARN};

/// Twinkle glyphs for particles, smallest → largest. The active glyph and
/// a dim/bright flag come from `(generation + phase)`.
const TWINKLE: [char; 3] = ['·', '•', '●'];

pub fn draw_scene(surface: &mut TextSurface, rect: Rect, scene: &StudioSceneModel) {
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }
    if scene.objects.is_empty() {
        // Empty-degrade: a scene with no objects leaves the background.
        return;
    }

    let bounds = Bounds::of(&scene.objects);
    let project = |position: [f32; 3]| -> Option<(usize, usize)> {
        // Apply camera pan/zoom in scene space first.
        let zoom = (scene.camera.fov_degrees / 45.0).max(0.1);
        let x = (position[0] - scene.camera.target[0]) * zoom;
        let y = (position[1] - scene.camera.target[1]) * zoom;
        let nx = bounds.norm_x(x);
        // +y is up in scene space; rows grow downward, so flip.
        let ny = 1.0 - bounds.norm_y(y);
        if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) {
            return None;
        }
        // Inset by 1 cell so edge objects stay on-screen.
        let col = rect.x as usize + (nx * (w.saturating_sub(1)) as f32).round() as usize;
        let row = rect.y as usize + (ny * (h.saturating_sub(1)) as f32).round() as usize;
        Some((
            col.min(rect.x as usize + w - 1),
            row.min(rect.y as usize + h - 1),
        ))
    };

    for (index, object) in scene.objects.iter().enumerate() {
        let Some((col, row)) = project(object.position) else {
            continue;
        };
        match object.kind {
            StudioSceneObjectKind::Text | StudioSceneObjectKind::LabelAnchor => {
                if let Some(label) = object.label.as_deref() {
                    draw_label(surface, rect, col, row, label, scene_style(object.tone));
                }
            }
            // Every non-text object draws as a point in the backdrop; the
            // atlas's structural kinds also degrade to dots here. Particles
            // twinkle; other points hold a steady glyph.
            _ => {
                let twinkles = matches!(object.kind, StudioSceneObjectKind::Particle);
                let phase = phase_for(&object.id, index);
                let (glyph, dim) = particle_glyph(object, scene.generation, phase, twinkles);
                let mut style = scene_style(object.tone);
                if dim {
                    style = style.fg(MUTED);
                }
                surface.draw_char(col, row, glyph, style);
            }
        }
    }
}

/// The particle glyph and a dim flag. Base size comes from `scale`;
/// twinkling particles cycle size and brightness by `generation + phase`.
fn particle_glyph(
    object: &StudioSceneObjectVm,
    generation: u64,
    phase: u64,
    twinkles: bool,
) -> (char, bool) {
    let base = size_index(object.scale[0]);
    if !twinkles {
        return (TWINKLE[base], object.opacity < 0.5);
    }
    // Cycle through a 4-step phase: the glyph size oscillates base-1..base+1
    // and brightness dims on the trough — a fade/size pulse that reads at
    // ~4fps where positional motion would be steppy.
    let step = ((generation.wrapping_add(phase)) % 4) as i32;
    let delta = match step {
        0 => 0,
        1 => 1,
        2 => 0,
        _ => -1,
    };
    let idx = (base as i32 + delta).clamp(0, (TWINKLE.len() - 1) as i32) as usize;
    let dim = step == 3 || object.opacity < 0.45;
    (TWINKLE[idx], dim)
}

fn size_index(scale: f32) -> usize {
    if scale >= 0.85 {
        2
    } else if scale >= 0.5 {
        1
    } else {
        0
    }
}

/// A stable per-object phase so particles twinkle out of unison.
fn phase_for(id: &str, index: usize) -> u64 {
    let mut hash = index as u64;
    for byte in id.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
    }
    hash % 4
}

/// Draw a text object centred horizontally on its projected column,
/// clamped to the rect.
fn draw_label(
    surface: &mut TextSurface,
    rect: Rect,
    col: usize,
    row: usize,
    label: &str,
    style: Style,
) {
    let width = display_width(label);
    let left = rect.x as usize;
    let right = left + rect.w as usize;
    let start = col
        .saturating_sub(width / 2)
        .max(left)
        .min(right.saturating_sub(width.min(rect.w as usize)));
    surface.draw_text(start, row, label, style);
}

/// Tone → style over the backdrop background (BG, not PANEL): the scene is
/// drawn on the empty center, not inside a tile. Colour is mapped, never
/// invented.
fn scene_style(tone: StudioTone) -> Style {
    let fg: Color = match tone {
        StudioTone::Good => GOOD,
        StudioTone::Warn => WARN,
        StudioTone::Danger => DANGER,
        StudioTone::Accent => ACCENT,
        StudioTone::Neutral => FG_SOFT,
    };
    Style::new().fg(fg).bg(BG)
}

/// Bounding box of object x/y positions, for fit-to-rect projection.
struct Bounds {
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

impl Bounds {
    fn of(objects: &[StudioSceneObjectVm]) -> Self {
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for object in objects {
            min_x = min_x.min(object.position[0]);
            max_x = max_x.max(object.position[0]);
            min_y = min_y.min(object.position[1]);
            max_y = max_y.max(object.position[1]);
        }
        if !min_x.is_finite() {
            min_x = -1.0;
            max_x = 1.0;
            min_y = -1.0;
            max_y = 1.0;
        }
        Self {
            min_x,
            max_x,
            min_y,
            max_y,
        }
    }

    fn norm_x(&self, x: f32) -> f32 {
        let span = (self.max_x - self.min_x).max(0.001);
        (x - self.min_x) / span
    }

    fn norm_y(&self, y: f32) -> f32 {
        let span = (self.max_y - self.min_y).max(0.001);
        (y - self.min_y) / span
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::studio::scene_model::build_shard_scene;

    fn render(generation: u64, w: u16, h: u16) -> TextSurface {
        let mut surface = TextSurface::new(w as usize, h as usize);
        let scene = build_shard_scene(generation);
        draw_scene(&mut surface, Rect::new(0, 0, w, h), &scene);
        surface
    }

    fn glyph_grid(surface: &TextSurface) -> Vec<char> {
        let mut cells = Vec::new();
        for y in 0..surface.height {
            for x in 0..surface.width {
                let ch = surface.get(x, y).rune;
                cells.push(if ch == '\0' { ' ' } else { ch });
            }
        }
        cells
    }

    #[test]
    fn shard_scene_renders_particles_and_text() {
        let grid: String = glyph_grid(&render(0, 48, 24)).into_iter().collect();
        assert!(grid.contains("RYE OS"), "brand text object renders");
        assert!(
            grid.contains('·') || grid.contains('•') || grid.contains('●'),
            "particles render as dots"
        );
    }

    #[test]
    fn empty_scene_degrades_to_nothing() {
        let mut surface = TextSurface::new(20, 10);
        let scene = StudioSceneModel::default();
        draw_scene(&mut surface, Rect::new(0, 0, 20, 10), &scene);
        // A scene with no objects leaves the surface untouched (default
        // blank cells) — the background fill stands.
        for y in 0..surface.height {
            for x in 0..surface.width {
                assert_eq!(surface.get(x, y).rune, ' ');
            }
        }
    }

    #[test]
    fn twinkle_differs_across_generations() {
        // The animation-pipeline regression test: stepping `generation`
        // through the renderer changes the particle cells (size/brightness
        // pulse). This is the proof the generation → scene → render loop
        // actually animates.
        let a = glyph_grid(&render(0, 48, 24));
        let b = glyph_grid(&render(1, 48, 24));
        assert_ne!(a, b, "particle cells must differ across generation 0 and 1");
    }
}
