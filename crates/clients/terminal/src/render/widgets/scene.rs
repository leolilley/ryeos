//! The generic scene renderer: ONE renderer for every `widget: scene`
//! (the backdrop, the atlas, any future scene). It draws whatever objects
//! a `RyeOsSceneModel` declares — it knows nothing about "the prism" or
//! "the starfield". The background is content; this is the closed
//! primitive that draws it.
//!
//! Contract (spec §"The scene → cell contract"):
//! - orthographic-project each object's `[x, y]` into the target rect
//!   (camera gives pan/zoom; +y is up, so rows flip);
//! - glyph per object: a `particle` → a cell off its named density ramp
//!   (dots `˙.·:∙•●` by default, `glyph: diamond` for `˙.·⋄◇◈◆`;
//!   intensity picks the level, `scale` biases it up, the sweep crest
//!   sparks as `✦`); a `text` → its `label`;
//! - an object with `end` (`to:` in content) is an EDGE: contiguous
//!   cells rasterized along the segment, each sample with its own
//!   breathe phase so light ripples along declared line-art;
//! - an object with `orbit:` (degrees per generation, sign = direction)
//!   revolves around the scene origin on a vertically squashed ring,
//!   dimming through the back half — motes circle a standing solid like
//!   a ring plane, glowing faintly through it when they pass behind;
//! - a `fill` object is a FILLED solid: every cell inside its SDF shape
//!   (`shape: prism|sphere`, dimensions in `scale`) gets a density from
//!   flat-faceted lighting under a slowly sweeping light, per-cell noise
//!   simmer, and the sweep band — the orb-style mass, rendered through
//!   the same ramps; silhouette cells use SDF distance as coverage so
//!   edges anti-alias through the ramp's faint end;
//! - projection preserves proportions: one scale for both axes (cell
//!   aspect corrected), centred — a declared diamond lands as a diamond
//!   in any rect;
//! - colour by `tone` via the theme — blends BETWEEN theme colours only,
//!   never invented hues;
//! - particles BREATHE: intensity follows a smooth per-generation curve
//!   with a per-object phase from the object id, expressed mostly as a
//!   colour blend toward/away from the background (colour interpolation
//!   reads smooth at low frame rates where positional motion is steppy),
//!   with the glyph size shifting only at the curve's extremes;
//! - an optional scene-level SWEEP (content-declared) rolls a diagonal
//!   band of brightness across the objects by `generation` — light
//!   catching a facet;
//! - the scene's `energy` (a real signal the builder maps in, e.g. live
//!   threads on the node) quickens pacing and lifts brightness, so an
//!   idle scene is calm and a working one visibly alive.
//!
//! NEXT (deeper fills): `draw_fill` covers prism/sphere SDFs with facet
//! shading, noise simmer, and coverage anti-aliasing. Still open:
//! braille (U+2800: 2×4 sub-pixels per cell) or shade-block (`░▒▓█`)
//! rasters for higher-resolution fills, more shapes (cluster, torus),
//! and break-apart transitions — a fill decomposing into particles on
//! navigation, which falls out naturally since fills and particles
//! already share the density→ramp→colour mapping.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Color, Style, TextSurface};
use ryeos_client_base::ui::scene_model::{
    RyeOsSceneModel, RyeOsSceneObjectKind, RyeOsSceneObjectVm, SceneCutoutAmountVm,
};
use ryeos_client_base::ui::view_model::RyeOsTone;

use super::super::text::display_width;
use super::super::theme::{mix_toward, ACCENT, BG, DANGER, FG, FG_SOFT, GOOD, MUTED, WARN};

// Glyph palette for later reaches (all single-cell-width, monospace-safe):
//   deeper dots:   ˙ . · : ∙ • ●           — the default ramp below
//   open circles:  ° ◦ ○ ◉ ●               — hollow→solid reads as "igniting"
//   diamonds:      ⋄ ◇ ◈ ◆  + glints ✧ ✦   — facet geometry / sweep crest
//   shade blocks:  ░ ▒ ▓ █                 — full-cell texture; the material
//                                            for a dense FILLED solid
//   braille:       ⠁ ⠃ ⠇ ⠧ ⠷ ⠿ ⡿ ⣿         — 2×4 sub-pixels per cell: density
//                                            AND sub-cell shape; near-smooth
//                                            outlines — the SDF fill's raster
//   quadrants:     ▖ ▗ ▘ ▝ ▚ ▞ ▌ ▐ ▀ ▄     — 2×2 placement for fill edges
// Avoid emoji-width / patchy glyphs (✨ ⟡ ⬤ ・): cell width must stay 1.

/// The density ramps, faintest → brightest. Intensity picks the level and
/// the object's size biases it upward, so a breathing cell walks its ramp
/// (`∙` ↔ `●` for a large solid-body dot, `˙` ↔ `·` for a far mote) instead of
/// flipping between two glyphs — depth comes from the character set as
/// much as from colour. An object opts into a named ramp via `glyph:`
/// (content's choice); the default is dots.
const RAMP_DOT: [char; 7] = ['˙', '.', '·', ':', '∙', '•', '●'];
const RAMP_DIAMOND: [char; 7] = ['˙', '.', '·', '⋄', '◇', '◈', '◆'];

/// The sweep crest's glyph: light catching a facet renders as a literal
/// spark, not just a brighter dot.
const GLINT: char = '✦';

/// Sixteen-step breathing curve — the per-particle intensity cycle.
/// Smooth rise and fall (not a square pulse): with intensity expressed
/// as a colour blend, this reads as organic breathing even at a slow
/// tick. Sixteen steps ≈ a 2s cycle at the backdrop tick — unhurried,
/// while still changing every frame.
const BREATHE: [f32; 16] = [
    0.30, 0.36, 0.45, 0.54, 0.62, 0.71, 0.80, 0.87, 0.92, 0.87, 0.80, 0.71, 0.62, 0.54, 0.45, 0.36,
];

pub fn draw_scene(surface: &mut TextSurface, rect: Rect, scene: &RyeOsSceneModel) {
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
    // Proportion-preserving projection: ONE scale for both axes,
    // corrected for the terminal cell's ~2:1 height:width (a scene unit
    // spans CELL_ASPECT columns per row), centred in the rect. Per-axis
    // fit would stretch declared shapes into whatever the rect happens
    // to be — a diamond must land as a diamond at any size.
    const CELL_ASPECT: f32 = 2.0;
    let zoom = (scene.camera.fov_degrees / 45.0).max(0.1);
    let span_x = ((bounds.max_x - bounds.min_x) * zoom).max(0.001);
    let span_y = ((bounds.max_y - bounds.min_y) * zoom).max(0.001);
    let fit_w = w.saturating_sub(1) as f32;
    let fit_h = h.saturating_sub(1) as f32;
    let fit_scale = (fit_w / (span_x * CELL_ASPECT)).min(fit_h / span_y);
    let width_scale = (fit_w / 58.0).clamp(1.0, 2.2);
    let scale = fit_scale.min(width_scale);
    let center_x = (bounds.min_x + bounds.max_x) / 2.0;
    let center_y = (bounds.min_y + bounds.max_y) / 2.0;
    let screen_col_offset = scene.screen_offset[0] * w as f32;
    let screen_row_offset = scene.screen_offset[1] * h as f32;
    let project = |position: [f32; 3]| -> Option<(usize, usize)> {
        let x = (position[0] - scene.camera.target[0] - center_x) * zoom;
        // +y is up in scene space; rows grow downward, so flip.
        let y = (position[1] - scene.camera.target[1] - center_y) * zoom;
        let col =
            (rect.x as f32 + (w - 1) as f32 / 2.0 + screen_col_offset + x * scale * CELL_ASPECT)
                .round();
        let row = (rect.y as f32 + (h - 1) as f32 / 2.0 + screen_row_offset - y * scale).round();
        if col < rect.x as f32
            || row < rect.y as f32
            || col > (rect.x as usize + w - 1) as f32
            || row > (rect.y as usize + h - 1) as f32
        {
            return None;
        }
        Some((col as usize, row as usize))
    };

    // Energy quickens the whole scene: pacing multiplier for the breathe
    // steps and the sweep traversal.
    let pace = 1 + (scene.energy.clamp(0.0, 1.0) * 2.0).round() as u64;

    // The sweep band's position this frame, in the scene's diagonal
    // coordinate (x + y): it enters below the low corner and exits past
    // the high one, once per (energy-shortened) period.
    let sweep_band = scene.sweep.as_ref().map(|sweep| {
        let min_d = bounds.min_x + bounds.min_y - sweep.width;
        let max_d = bounds.max_x + bounds.max_y + sweep.width;
        let period = ((sweep.period as f32 / (1.0 + scene.energy.clamp(0.0, 1.0))) as u64).max(4);
        let progress = (scene.generation % period) as f32 / period as f32;
        (min_d + progress * (max_d - min_d), sweep.width)
    });

    // Fill pass first: solids are the mass everything else rides on
    // (edges, motes, and text draw over them).
    for object in &scene.objects {
        if matches!(object.kind, RyeOsSceneObjectKind::Fill) {
            draw_fill(
                surface,
                rect,
                scene,
                object,
                scale,
                CELL_ASPECT,
                (center_x, center_y),
                zoom,
                sweep_band,
                pace,
            );
        }
    }

    for (index, object) in scene.objects.iter().enumerate() {
        // Orbiting objects revolve on the squashed ring; everything else
        // sits where declared. `depth` is the ring front-ness (1 = front,
        // 0 = passing behind the scene's mass).
        let (position, depth) = orbited_position(object, scene, pace);
        let Some((col, row)) = project(position) else {
            continue;
        };
        match object.kind {
            RyeOsSceneObjectKind::Fill => {}
            RyeOsSceneObjectKind::Text | RyeOsSceneObjectKind::LabelAnchor => {
                if let Some(label) = object.label.as_deref() {
                    draw_label(surface, rect, col, row, label, scene_style(object.tone));
                }
            }
            // Every non-text object draws as cells in the backdrop; the
            // atlas's structural kinds also degrade to dots here. An
            // object with `end` is an EDGE — contiguous cells rasterized
            // along the segment (light ripples along it: each sample gets
            // its own breathe phase). Particles breathe; other kinds hold
            // a steady glyph.
            _ => {
                let base_phase = phase_for(&object.id, index);
                let breathes = matches!(object.kind, RyeOsSceneObjectKind::Particle);
                let steady_cell = || -> (char, Style) {
                    let mut style = scene_style(object.tone);
                    if object.opacity < 0.5 {
                        style = style.fg(MUTED);
                    }
                    (ramp_for(object)[steady_level(object.scale[0])], style)
                };
                if let Some(end) = object.end {
                    let mut end = match object.orbit {
                        Some(speed) => orbit_point(end, speed, scene.generation, pace).0,
                        None => end,
                    };
                    let offset = break_offset(object, scene, pace);
                    end[0] += offset[0];
                    end[1] += offset[1];
                    end[2] += offset[2];
                    let Some((end_col, end_row)) = project(end) else {
                        continue;
                    };
                    let steps = (end_col as i64 - col as i64)
                        .abs()
                        .max((end_row as i64 - row as i64).abs())
                        .max(1);
                    for i in 0..=steps {
                        let t = i as f32 / steps as f32;
                        let cell_col =
                            (col as f32 + (end_col as f32 - col as f32) * t).round() as usize;
                        let cell_row =
                            (row as f32 + (end_row as f32 - row as f32) * t).round() as usize;
                        let pos = [
                            position[0] + (end[0] - position[0]) * t,
                            position[1] + (end[1] - position[1]) * t,
                            0.0,
                        ];
                        let (glyph, style) = if breathes {
                            particle_cell(
                                object,
                                scene,
                                base_phase.wrapping_add(i as u64),
                                pace,
                                sweep_band,
                                pos,
                                depth,
                            )
                        } else {
                            steady_cell()
                        };
                        surface.draw_char(cell_col, cell_row, glyph, style);
                    }
                } else {
                    let (glyph, style) = if breathes {
                        particle_cell(object, scene, base_phase, pace, sweep_band, position, depth)
                    } else {
                        steady_cell()
                    };
                    surface.draw_char(col, row, glyph, style);
                }
            }
        }
    }
}

/// The ring plane's vertical squash: orbits are circles in scene space
/// flattened to read as a plane seen edge-on around a standing solid.
const ORBIT_SQUASH: f32 = 0.45;

/// Where an orbiting object is this frame, plus its front-ness in
/// `[0, 1]` (1 = nearest the viewer, 0 = passing behind). The declared
/// position fixes the ring radius and starting phase, so generation 0
/// renders the scene exactly as authored. Break motion is folded into the
/// point before orbiting so split shards can revolve around the center
/// instead of only pulsing in fixed columns.
fn orbited_position(
    object: &RyeOsSceneObjectVm,
    scene: &RyeOsSceneModel,
    pace: u64,
) -> ([f32; 3], f32) {
    let offset = break_offset(object, scene, pace);
    let position = [
        object.position[0] + offset[0],
        object.position[1] + offset[1],
        object.position[2] + offset[2],
    ];
    match object.orbit {
        Some(speed) => orbit_point(position, speed, scene.generation, pace),
        None => (position, 1.0),
    }
}

fn break_offset(object: &RyeOsSceneObjectVm, scene: &RyeOsSceneModel, pace: u64) -> [f32; 3] {
    let Some(motion) = object.break_motion else {
        return [0.0, 0.0, 0.0];
    };
    let period = motion.period.max(4);
    let step = scene
        .generation
        .wrapping_mul(pace)
        .wrapping_add(motion.phase)
        % period;
    let progress = step as f32 / period as f32;
    let eased =
        (0.5 - 0.5 * (progress * std::f32::consts::TAU).cos()) * scene.break_amount.clamp(0.0, 1.0);
    [
        motion.away[0] * eased,
        motion.away[1] * eased,
        motion.away[2] * eased,
    ]
}

fn local_clip_allows(object: &RyeOsSceneObjectVm, lx: f32, ly: f32) -> bool {
    let Some(clip) = object.clip else {
        return true;
    };
    if clip.x_min.map_or(false, |min| lx < min) {
        return false;
    }
    if clip.x_max.map_or(false, |max| lx > max) {
        return false;
    }
    if clip.y_min.map_or(false, |min| ly < min) {
        return false;
    }
    if clip.y_max.map_or(false, |max| ly > max) {
        return false;
    }
    true
}

fn orbit_point(point: [f32; 3], speed: f32, generation: u64, pace: u64) -> ([f32; 3], f32) {
    let ring_y = point[1] / ORBIT_SQUASH;
    let radius = (point[0] * point[0] + ring_y * ring_y).sqrt();
    let start = ring_y.atan2(point[0]);
    // Accumulate the angle in f64 modulo a full turn so precision holds
    // over long sessions.
    let swept = ((speed as f64) * (generation.wrapping_mul(pace) as f64)).rem_euclid(360.0) as f32;
    let theta = start + swept.to_radians();
    let (sin, cos) = theta.sin_cos();
    // The ring's back half (positive sin) is farther from the viewer.
    let depth = (1.0 - sin) / 2.0;
    ([radius * cos, ORBIT_SQUASH * radius * sin, point[2]], depth)
}

/// One breathing particle's glyph + style for this frame.
///
/// Intensity = breathe curve (generation-stepped, phase-desynced, energy-
/// paced) + sweep-band boost + energy floor, scaled by the object's
/// opacity. It expresses mostly as colour: the tone colour recedes toward
/// the background as intensity falls, and a strong sweep glint lifts it
/// toward the foreground — light rolling across a facet. The glyph only
/// steps around its base size at the curve's extremes.
fn particle_cell(
    object: &RyeOsSceneObjectVm,
    scene: &RyeOsSceneModel,
    phase: u64,
    pace: u64,
    sweep_band: Option<(f32, f32)>,
    position: [f32; 3],
    depth: f32,
) -> (char, Style) {
    let energy = scene.energy.clamp(0.0, 1.0);
    let step = scene.generation.wrapping_mul(pace).wrapping_add(phase);
    let breathe = BREATHE[(step % BREATHE.len() as u64) as usize];
    let boost = sweep_band
        .map(|(band, width)| {
            let d = position[0] + position[1];
            (1.0 - ((d - band).abs() / width.max(0.001))).max(0.0)
        })
        .unwrap_or(0.0);
    let reveal = reveal_multiplier(object, scene, pace);
    // Ring depth dims the back half of an orbit — a mote passing behind
    // the scene's mass glows faintly through it, never occludes it.
    let intensity = ((breathe * (0.55 + 0.25 * energy)) + boost * 0.65 + energy * 0.10)
        .clamp(0.0, 1.0)
        * object.opacity.clamp(0.1, 1.0)
        * reveal
        * (0.55 + 0.45 * depth.clamp(0.0, 1.0));

    // Intensity walks the whole ramp; size biases the walk upward. A big
    // large body diamond breathes `◈` ↔ `◆`, a mid facet edge `⋄` ↔ `◇`,
    // a far mote `˙` ↔ `·` — and the sweep crest pushes any of them to
    // the ramp's top.
    let ramp = ramp_for(object);
    let level = ((intensity * 6.0).round() as i32 + size_bias(object.scale[0]))
        .clamp(0, (ramp.len() - 1) as i32) as usize;

    let tone = scene_style(object.tone);
    let mut fg = mix_toward(tone_color(object.tone), BG, 0.75 * (1.0 - intensity));
    let glyph = if boost > 0.7 {
        // The glint: the band's crest is a literal spark, its tone washed
        // toward the page foreground for a beat.
        fg = mix_toward(fg, FG, ((boost - 0.7) * 2.0).min(0.6));
        GLINT
    } else {
        ramp[level]
    };
    (glyph, tone.fg(fg).bg(BG))
}

/// Rasterize a filled solid: for every cell in the rect, inverse-map to
/// scene coordinates, sample the object's SDF shape, and draw a density
/// cell — flat-faceted shading under a light that sweeps side to side
/// like a lighthouse (energy quickens it), per-cell noise simmer, sweep
/// boost, coverage-based edge anti-aliasing. The orb lesson applied: a
/// filled mass reads statistically, so shading and texture carry the
/// form and no cell has to be individually perfect.
#[allow(clippy::too_many_arguments)]
fn draw_fill(
    surface: &mut TextSurface,
    rect: Rect,
    scene: &RyeOsSceneModel,
    object: &RyeOsSceneObjectVm,
    scale: f32,
    cell_aspect: f32,
    center: (f32, f32),
    zoom: f32,
    sweep_band: Option<(f32, f32)>,
    pace: u64,
) {
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }
    let energy = scene.energy.clamp(0.0, 1.0);
    // The light oscillates across the front arc rather than orbiting
    // behind (a fully dark solid reads as a hole, not drama).
    let light_phase =
        (scene.generation.wrapping_mul(pace) % 224) as f32 / 224.0 * std::f32::consts::TAU;
    let light_az = 1.55 * light_phase.sin();
    // The shape's own rotation about its vertical axis (content-declared
    // `spin:`): facet seams sweep across the face and each facet rolls
    // through the light. Accumulated in f64 modulo a full turn so
    // precision holds over long sessions.
    let spin = ((object.spin.unwrap_or(0.0) as f64 * scene.generation.wrapping_mul(pace) as f64)
        .rem_euclid(360.0) as f32)
        .to_radians();
    // One cell's width in scene units: the anti-alias band.
    let soft = 1.0 / (scale * cell_aspect * zoom).max(0.001);
    let ramp = ramp_for(object);
    let noise_amp = 0.10 + 0.12 * energy;
    let opacity = object.opacity.clamp(0.0, 1.0) * reveal_multiplier(object, scene, pace);
    let (position, _depth) = orbited_position(object, scene, pace);

    for row in 0..h {
        for col in 0..w {
            // Inverse of `project`: cell → scene coordinates.
            let x = (col as f32 - (w - 1) as f32 / 2.0 - scene.screen_offset[0] * w as f32)
                / (scale * cell_aspect * zoom)
                + center.0
                + scene.camera.target[0];
            let y = ((h - 1) as f32 / 2.0 + scene.screen_offset[1] * h as f32 - row as f32)
                / (scale * zoom)
                + center.1
                + scene.camera.target[1];
            let lx = x - position[0];
            let ly = y - position[1];
            if !local_clip_allows(object, lx, ly) {
                continue;
            }
            if local_cutout_blocks(object, scene, lx, ly, light_az, spin) {
                continue;
            }

            let (sd, shade) = match object.shape.as_deref() {
                Some("sphere") => sphere_sample(object.scale[0], lx, ly, light_az),
                _ => prism_sample(
                    object.scale[0],
                    object.scale[1],
                    object.scale[2],
                    lx,
                    ly,
                    light_az,
                    spin,
                ),
            };
            let coverage = (0.5 - sd / soft).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let boost = sweep_band
                .map(|(band, width)| (1.0 - (((x + y) - band).abs() / width.max(0.001))).max(0.0))
                .unwrap_or(0.0);
            let noise = hash_noise(col, row, scene.generation / 3);
            // TASTE KNOBS — tune these before touching anything else:
            // 0.34 = ambient floor (how visible the dark faces stay),
            // 0.62 = facet weight (how hard lit/unlit faces contrast),
            // 0.30 = sweep lift, and `noise_amp` above = surface grain.
            let density = ((coverage * (0.34 + 0.62 * shade + 0.08 * energy))
                + boost * 0.30
                + (noise - 0.5) * noise_amp)
                .clamp(0.0, 1.0)
                * opacity;
            if density < 0.04 {
                continue;
            }
            let level = ((density * 6.0).round() as i32).clamp(0, 6) as usize;
            let mut fg = mix_toward(tone_color(object.tone), BG, 0.62 * (1.0 - density));
            let glyph = if boost > 0.7 && density > 0.75 && noise > 0.78 {
                fg = mix_toward(fg, FG, 0.5);
                GLINT
            } else {
                ramp[level]
            };
            surface.draw_char(
                rect.x as usize + col,
                rect.y as usize + row,
                glyph,
                Style::new().fg(fg).bg(BG),
            );
        }
    }
}

fn reveal_multiplier(object: &RyeOsSceneObjectVm, scene: &RyeOsSceneModel, pace: u64) -> f32 {
    let Some(reveal) = object.reveal else {
        return 1.0;
    };
    let period = reveal.period.max(4);
    let step = scene
        .generation
        .wrapping_mul(pace)
        .wrapping_add(reveal.phase)
        % period;
    let progress = step as f32 / period as f32;
    let closed = 0.5 + 0.5 * (progress * std::f32::consts::TAU).cos();
    let sharpness = reveal.sharpness.max(0.1);
    let floor = reveal.floor.clamp(0.0, 1.0);
    (floor + (1.0 - floor) * closed.powf(sharpness)) * scene.break_amount.clamp(0.0, 1.0)
}

fn local_cutout_blocks(
    object: &RyeOsSceneObjectVm,
    scene: &RyeOsSceneModel,
    lx: f32,
    ly: f32,
    light_az: f32,
    spin: f32,
) -> bool {
    object.cutouts.iter().any(|cutout| {
        let amount = match cutout.amount {
            SceneCutoutAmountVm::Static => 1.0,
            SceneCutoutAmountVm::Break => scene.break_amount.clamp(0.0, 1.0),
            SceneCutoutAmountVm::BreakSpin => {
                let facing = 0.5 + 0.5 * spin.cos();
                let gate = ((facing - 0.22) / 0.38).clamp(0.0, 1.0);
                scene.break_amount.clamp(0.0, 1.0) * gate
            }
        };
        if amount <= 0.03 {
            return false;
        }
        let x = lx - cutout.position[0];
        let y = ly - cutout.position[1];
        let sx = cutout.scale[0] * amount;
        let sy = cutout.scale[1] * amount;
        let sz = cutout.scale[2] * amount;
        let (sd, _) = match cutout.shape.as_deref() {
            Some("sphere") => sphere_sample(sx, x, y, light_az),
            _ => prism_sample(sx, sy, sz, x, y, light_az, spin),
        };
        sd <= 0.0
    })
}

/// Prism SDF + shading: a hexagonal crystal column (radius `r`, body
/// half-height `bh`) terminating in a pyramidal point (`tip` tall) at
/// BOTH ends — a double-terminated crystal. Returns (signed distance in
/// scene units, facet shade). The front-half azimuth across the local
/// width quantizes to hex faces; each face is one flat brightness
/// against the light — crisp internal facet seams come from the shading
/// discontinuities, no drawn lines.
fn prism_sample(
    r: f32,
    bh: f32,
    tip: f32,
    lx: f32,
    ly: f32,
    light_az: f32,
    spin: f32,
) -> (f32, f32) {
    use std::f32::consts::{FRAC_PI_2, PI};
    // Silhouette half-width at this height: the column, tapering to a
    // point across the termination at each end.
    let rw = if ly.abs() <= bh {
        r
    } else {
        r * (1.0 - ((ly.abs() - bh) / tip.max(0.001)).min(1.0))
    };
    let sd = (lx.abs() - rw).max(ly.abs() - (bh + tip));

    // Facet lookup happens in the CRYSTAL's frame (viewer azimuth +
    // spin), but the face normal shades in the VIEWER's frame — so as
    // the crystal spins, seams sweep across the face and each face's
    // brightness rolls continuously through the light.
    let az = (lx / rw.max(0.15)).clamp(-1.0, 1.0).asin();
    let sector = ((az + spin + FRAC_PI_2) / (PI / 3.0)).floor();
    let face_az = sector * (PI / 3.0) + PI / 6.0 - FRAC_PI_2 - spin;
    let mut shade = (face_az - light_az).cos().max(0.0);
    if ly > bh {
        // The upper termination's faces tilt skyward: the point catches
        // light.
        shade = (shade + 0.25 * ((ly - bh) / tip.max(0.001))).min(1.2);
    } else if ly < -bh {
        // The lower termination turns away from the raised light.
        shade *= 0.9;
    }
    // Grounding: slightly darker toward the base.
    let vertical = 0.88 + 0.17 * ((ly + bh + tip) / (2.0 * (bh + tip)).max(0.001)).clamp(0.0, 1.0);
    (sd, shade * vertical)
}

/// Sphere SDF + Lambert shading under the same oscillating light (fixed
/// upward elevation) — the orb form, available to any scene content.
fn sphere_sample(r: f32, lx: f32, ly: f32, light_az: f32) -> (f32, f32) {
    let r = r.max(0.001);
    let d = (lx * lx + ly * ly).sqrt();
    let sd = d - r;
    let nx = (lx / r).clamp(-1.0, 1.0);
    let ny = (ly / r).clamp(-1.0, 1.0);
    let nz = (1.0 - nx * nx - ny * ny).max(0.0).sqrt();
    // Light: oscillating azimuth, fixed 35° elevation.
    let elevation = 0.61f32;
    let (le_sin, le_cos) = elevation.sin_cos();
    let light = [le_cos * light_az.sin(), le_sin, le_cos * light_az.cos()];
    let shade = (nx * light[0] + ny * light[1] + nz * light[2]).max(0.0);
    (sd, shade)
}

/// Deterministic per-cell noise in `[0, 1]` — the simmer texture. Seeded
/// by generation (a third: ~2.7Hz at the backdrop tick) so the surface
/// grains without strobing.
fn hash_noise(col: usize, row: usize, seed: u64) -> f32 {
    let mut h = (col as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (row as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ seed.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Resolve an object's named glyph ramp (content's choice; dots default).
fn ramp_for(object: &RyeOsSceneObjectVm) -> &'static [char; 7] {
    match object.glyph.as_deref() {
        Some("diamond") => &RAMP_DIAMOND,
        _ => &RAMP_DOT,
    }
}

/// Size class from `scale`: how far up the density ramp an object's
/// breathing range sits (0 = faint far mote, 3 = large body).
fn size_bias(scale: f32) -> i32 {
    if scale >= 0.85 {
        3
    } else if scale >= 0.5 {
        2
    } else {
        0
    }
}

/// Steady (non-breathing) point glyph level for a size class — mid-ramp
/// and up, matching the old `·`/`•`/`●` weights on the dot ramp.
fn steady_level(scale: f32) -> usize {
    match size_bias(scale) {
        3 => 6,
        2 => 5,
        _ => 2,
    }
}

/// A stable per-object phase so particles breathe out of unison.
fn phase_for(id: &str, index: usize) -> u64 {
    let mut hash = index as u64;
    for byte in id.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
    }
    hash % BREATHE.len() as u64
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

/// Tone → theme colour. Colour is mapped, never invented; animation only
/// ever blends between these mapped colours and the background.
fn tone_color(tone: RyeOsTone) -> Color {
    match tone {
        RyeOsTone::Good => GOOD,
        RyeOsTone::Warn => WARN,
        RyeOsTone::Danger => DANGER,
        RyeOsTone::Accent => ACCENT,
        RyeOsTone::Neutral => FG_SOFT,
    }
}

/// Tone → style over the backdrop background (BG, not PANEL): the scene is
/// drawn on the empty center, not inside a tile.
fn scene_style(tone: RyeOsTone) -> Style {
    Style::new().fg(tone_color(tone)).bg(BG)
}

/// Bounding box of object x/y positions, for fit-to-rect projection.
struct Bounds {
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

impl Bounds {
    fn of(objects: &[RyeOsSceneObjectVm]) -> Self {
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        let mut consider = |point: [f32; 3]| {
            min_x = min_x.min(point[0]);
            max_x = max_x.max(point[0]);
            min_y = min_y.min(point[1]);
            max_y = max_y.max(point[1]);
        };
        for object in objects {
            if !object.fit {
                continue;
            }
            if object.orbit.is_some() {
                // An orbiting object sweeps its whole ring: bound the
                // ring, not today's spot, so layout never jitters as it
                // moves.
                let ring_y = object.position[1] / ORBIT_SQUASH;
                let radius = (object.position[0] * object.position[0] + ring_y * ring_y).sqrt();
                consider([-radius, -ORBIT_SQUASH * radius, 0.0]);
                consider([radius, ORBIT_SQUASH * radius, 0.0]);
            } else {
                consider(object.position);
            }
            if let Some(end) = object.end {
                consider(end);
            }
            if matches!(object.kind, RyeOsSceneObjectKind::Fill) {
                // A fill's extent is its shape's, not its centre point:
                // radius wide, body + termination each way vertically.
                let reach = object.scale[1] + object.scale[2];
                consider([
                    object.position[0] - object.scale[0],
                    object.position[1] - reach,
                    0.0,
                ]);
                consider([
                    object.position[0] + object.scale[0],
                    object.position[1] + reach,
                    0.0,
                ]);
                if let Some(motion) = object.break_motion {
                    consider([
                        object.position[0] + motion.away[0] - object.scale[0],
                        object.position[1] + motion.away[1] - reach,
                        0.0,
                    ]);
                    consider([
                        object.position[0] + motion.away[0] + object.scale[0],
                        object.position[1] + motion.away[1] + reach,
                        0.0,
                    ]);
                }
            } else if let Some(motion) = object.break_motion {
                consider([
                    object.position[0] + motion.away[0],
                    object.position[1] + motion.away[1],
                    object.position[2] + motion.away[2],
                ]);
            }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::scene_model::scene_from_body;

    fn render(generation: u64, w: u16, h: u16) -> TextSurface {
        // A content-shaped scene (particles + a brand text object), the
        // same path the backdrop view uses.
        let body = serde_json::json!({
            "objects": [
                { "kind": "particle", "position": [0.0, 6.0], "scale": 0.9, "color": "#d65d0e", "tone": "accent" },
                { "kind": "particle", "position": [3.0, -1.2], "scale": 0.9, "color": "#d65d0e", "tone": "accent" },
                { "kind": "particle", "position": [-9.0, -3.5], "scale": 0.6, "color": "#8ec07c", "tone": "good" },
                { "kind": "particle", "position": [10.0, 3.4], "scale": 0.5, "color": "#a89984", "tone": "neutral" },
                { "kind": "text", "position": [0.0, -8.2], "label": "RYE OS", "color": "#d65d0e", "tone": "accent" }
            ]
        });
        let mut surface = TextSurface::new(w as usize, h as usize);
        let scene = scene_from_body(&body, generation);
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
    fn particle_scene_renders_particles_and_text() {
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
        let scene = RyeOsSceneModel::default();
        draw_scene(&mut surface, Rect::new(0, 0, 20, 10), &scene);
        // A scene with no objects leaves the surface untouched (default
        // blank cells) — the background fill stands.
        for y in 0..surface.height {
            for x in 0..surface.width {
                assert_eq!(surface.get(x, y).rune, ' ');
            }
        }
    }

    fn styled_grid(surface: &TextSurface) -> Vec<(char, Color)> {
        let mut cells = Vec::new();
        for y in 0..surface.height {
            for x in 0..surface.width {
                let cell = surface.get(x, y);
                let ch = if cell.rune == '\0' { ' ' } else { cell.rune };
                cells.push((ch, cell.fg));
            }
        }
        cells
    }

    #[test]
    fn twinkle_differs_across_generations() {
        // The animation-pipeline regression test: stepping `generation`
        // through the renderer changes the particle cells. The breathe is
        // primarily a colour blend (glyph size steps only at the curve's
        // extremes), so the comparison includes each cell's fg colour.
        let a = styled_grid(&render(0, 48, 24));
        let b = styled_grid(&render(1, 48, 24));
        assert_ne!(a, b, "particle cells must differ across generation 0 and 1");
    }

    #[test]
    fn edge_objects_rasterize_contiguous_cells() {
        // An object with `to:` is a segment: the renderer fills the cells
        // between its endpoints, so declared line-art stays a line at any
        // terminal size instead of two lonely dots.
        let body = serde_json::json!({
            "objects": [
                { "kind": "particle", "position": [-5.0, 0.0], "to": [5.0, 0.0],
                  "scale": 0.9, "tone": "accent" },
            ],
        });
        let mut surface = TextSurface::new(40, 10);
        let scene = scene_from_body(&body, 0);
        draw_scene(&mut surface, Rect::new(0, 0, 40, 10), &scene);
        let filled: usize = (0..surface.width)
            .filter(|x| {
                (0..surface.height).any(|y| {
                    let rune = surface.get(*x, y).rune;
                    rune != '\0' && rune != ' '
                })
            })
            .count();
        assert!(
            filled >= 30,
            "a horizontal edge across a 40-cell rect fills most columns, got {filled}"
        );
    }

    #[test]
    fn orbiting_particle_moves_around_the_ring() {
        // An `orbit:` object's drawn cell travels across generations
        // (its declared position is only the starting phase), while the
        // scene's bounds stay pinned to the whole ring so nothing else
        // jitters as it moves.
        let body = serde_json::json!({
            "objects": [
                { "kind": "particle", "position": [8.0, 0.0], "orbit": 3.0,
                  "scale": 0.9, "tone": "accent" },
            ],
        });
        let cells_at = |generation: u64| -> Vec<(usize, usize)> {
            let mut surface = TextSurface::new(40, 16);
            let scene = scene_from_body(&body, generation);
            draw_scene(&mut surface, Rect::new(0, 0, 40, 16), &scene);
            (0..surface.width)
                .flat_map(|x| (0..surface.height).map(move |y| (x, y)))
                .filter(|(x, y)| {
                    let rune = surface.get(*x, *y).rune;
                    rune != '\0' && rune != ' '
                })
                .collect()
        };
        assert_ne!(
            cells_at(0),
            cells_at(30),
            "90° of orbit must relocate the particle's cell"
        );
    }

    #[test]
    fn fill_prism_rasterizes_a_solid_mass_that_animates() {
        // A `fill` object shades every interior cell (the orb-style
        // mass), and the oscillating light + noise simmer change cells
        // across generations.
        let body = serde_json::json!({
            "objects": [
                { "kind": "fill", "shape": "prism", "position": [0.0, 0.0],
                  "scale": [3.0, 3.6, 2.6], "tone": "accent" },
            ],
        });
        let frame = |generation: u64| {
            let mut surface = TextSurface::new(48, 24);
            let scene = scene_from_body(&body, generation);
            draw_scene(&mut surface, Rect::new(0, 0, 48, 24), &scene);
            surface
        };
        let a = frame(0);
        let filled = (0..a.width)
            .flat_map(|x| (0..a.height).map(move |y| (x, y)))
            .filter(|(x, y)| {
                let rune = a.get(*x, *y).rune;
                rune != '\0' && rune != ' '
            })
            .count();
        assert!(
            filled > 150,
            "a filled prism covers a solid interior, got {filled} cells"
        );
        assert_ne!(
            styled_grid(&a),
            styled_grid(&frame(7)),
            "the fill's lighting/simmer must move across generations"
        );
    }

    #[test]
    fn sweep_band_lifts_particles_it_crosses() {
        // A scene declaring `sweep` renders a particle brighter (fg nearer
        // the tone colour) when the band sits on it than when it is far
        // away — the light-across-the-facet effect exists and moves.
        let body = serde_json::json!({
            "sweep": { "period": 8, "width": 3.0 },
            "objects": [
                { "kind": "particle", "position": [-6.0, 0.0], "scale": 0.9, "tone": "accent" },
                { "kind": "particle", "position": [6.0, 0.0], "scale": 0.9, "tone": "accent" },
            ],
        });
        let mut frames: Vec<Vec<(char, Color)>> = Vec::new();
        for generation in 0..8 {
            let mut surface = TextSurface::new(48, 12);
            let scene = scene_from_body(&body, generation);
            draw_scene(&mut surface, Rect::new(0, 0, 48, 12), &scene);
            frames.push(styled_grid(&surface));
        }
        // Across one full period the frames are not all identical, and at
        // least two distinct colourings of the same cells appear.
        let distinct: std::collections::BTreeSet<String> =
            frames.iter().map(|f| format!("{f:?}")).collect();
        assert!(
            distinct.len() >= 2,
            "sweep must vary the rendered cells across its period"
        );
    }
}
