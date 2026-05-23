//! Distance-field substrate renderer.
//!
//! Inspired by opencode's bg-pulse-render: precompute a distance grid from
//! center, then sweep pulse waves through it. Zero 3D math per frame.
//! The output is a flat array of RGB cell colors — both terminal (ANSI bg)
//! and web (Canvas rectangles) just stamp these colors.
//!
//! The animation is a sum of:
//!   - Multiple ring pulses with cosine crests and power-law tails
//!   - A global "breath" oscillation
//!   - Edge falloff so pulses fade at screen borders
//!
//! Everything is a pure function of (distance, time) — no per-frame projection.

use crate::scene::Rgb;
use crate::store::Store;

/// Substrate mode — controls pulse intensity and color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstrateMode {
    Idle,
    Active,
    ErrorPulse,
}

/// Configuration for the substrate renderer.
#[derive(Debug, Clone)]
pub struct SubstrateConfig {
    // Pulse geometry
    pub period_ms: f32,
    pub rings: usize,
    pub ring_width: f32,
    pub tail_length: f32,
    pub amplitude: f32,
    pub tail_amplitude: f32,
    pub breath_amplitude: f32,
    pub breath_speed: f32,
    pub phase_offset: f32,

    // Colors
    pub bg_color: Rgb,
    pub pulse_color: Rgb,
    pub error_color: Rgb,

    // Aspect ratio compensation (terminal chars are ~2:1 wide:tall)
    pub y_scale: f32,
}

impl Default for SubstrateConfig {
    fn default() -> Self {
        Self {
            period_ms: 4600.0,
            rings: 3,
            ring_width: 3.8,
            tail_length: 9.5,
            amplitude: 0.55,
            tail_amplitude: 0.16,
            breath_amplitude: 0.05,
            breath_speed: 0.0008,
            phase_offset: 0.29,
            bg_color: Rgb::new(0x1d, 0x20, 0x21), // Gruvbox dark
            pulse_color: Rgb::new(0xfe, 0x80, 0x19), // Gruvbox orange
            error_color: Rgb::new(0xfb, 0x49, 0x34), // Gruvbox red
            y_scale: 2.0, // Terminal chars are twice as tall as wide
        }
    }
}

/// Precomputed distance field + runtime state for the substrate.
#[derive(Debug, Clone)]
pub struct SubstrateState {
    config: SubstrateConfig,
    mode: SubstrateMode,
    activity_intensity: f32,

    // Precomputed geometry
    distances: Vec<f32>,
    edge_falloff: Vec<f32>,
    pub width: usize,
    pub height: usize,
    reach: f32,

    // Time
    elapsed_ms: f32,

    // Output buffer (reused across frames)
    cells: Vec<Rgb>,
}

impl SubstrateState {
    pub fn new(config: SubstrateConfig) -> Self {
        Self {
            config,
            mode: SubstrateMode::Idle,
            activity_intensity: 0.0,
            distances: Vec::new(),
            edge_falloff: Vec::new(),
            width: 0,
            height: 0,
            reach: 1.0,
            elapsed_ms: 0.0,
            cells: Vec::new(),
        }
    }

    /// Advance time by delta_ms and update mode from store.
    pub fn tick(&mut self, delta_ms: u64, store: &Store) {
        self.elapsed_ms = (self.elapsed_ms + delta_ms as f32) % self.config.period_ms;

        // Determine mode from store
        let running = store.running_thread_count();
        let has_errors = store
            .trust_alerts
            .iter()
            .any(|a| matches!(a.severity, crate::store::TrustSeverity::Error));

        let target_mode = if has_errors {
            SubstrateMode::ErrorPulse
        } else if running > 0 {
            SubstrateMode::Active
        } else {
            SubstrateMode::Idle
        };

        self.mode = target_mode;

        let target_intensity = match self.mode {
            SubstrateMode::Idle => 0.3,
            SubstrateMode::Active => 0.7,
            SubstrateMode::ErrorPulse => 0.9,
        };
        self.activity_intensity += (target_intensity - self.activity_intensity) * 0.05;
    }

    /// Current mode.
    pub fn mode(&self) -> SubstrateMode {
        self.mode
    }

    /// Ensure the distance field matches the given dimensions.
    /// Call this before render() if the viewport might have changed.
    pub fn ensure_geometry(&mut self, width: usize, height: usize) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;

        let center_x = width as f32 / 2.0;
        let center_y = height as f32 / 2.0;
        let y_scale = self.config.y_scale;

        // Reach = max distance from center to any corner
        let cx = center_x.max(width as f32 - center_x);
        let cy = center_y.max(height as f32 - center_y) * y_scale;
        self.reach = cx.hypot(cy) + self.config.tail_length;

        let len = width * height;
        self.distances = Vec::with_capacity(len);
        self.edge_falloff = Vec::with_capacity(len);
        self.cells = vec![self.config.bg_color; len];

        for y in 0..height {
            for x in 0..width {
                let dist = (x as f32 + 0.5 - center_x)
                    .hypot((y as f32 + 0.5 - center_y) * y_scale);
                self.distances.push(dist);
                let edge = 1.0 - (dist / (self.reach * 0.85)).powi(2);
                self.edge_falloff.push(edge.max(0.0));
            }
        }
    }

    /// Render the substrate into the internal cell buffer.
    /// Returns a slice of RGB colors, one per cell (row-major, width×height).
    pub fn render(&mut self) -> &[Rgb] {
        if self.width == 0 || self.height == 0 {
            return &self.cells;
        }

        let t = self.elapsed_ms;
        let cfg = &self.config;
        let ring_count = cfg.rings;
        let period = cfg.period_ms;
        let intensity = self.activity_intensity;

        // Choose colors based on mode
        let (base_r, base_g, base_b) = match self.mode {
            SubstrateMode::ErrorPulse => {
                let pulse = (t / 500.0).sin() * 0.3 + 0.7;
                let bg = cfg.bg_color;
                let err = cfg.error_color;
                (
                    lerp(bg.r as f32, err.r as f32, pulse),
                    lerp(bg.g as f32, err.g as f32, pulse * 0.3),
                    lerp(bg.b as f32, err.b as f32, pulse * 0.2),
                )
            }
            _ => (cfg.bg_color.r as f32, cfg.bg_color.g as f32, cfg.bg_color.b as f32),
        };

        let pulse = cfg.pulse_color;
        let delta_r = pulse.r as f32 - base_r;
        let delta_g = pulse.g as f32 - base_g;
        let delta_b = pulse.b as f32 - base_b;

        // Global breath
        let breath = (0.5 + 0.5 * (t * cfg.breath_speed).sin()) * cfg.breath_amplitude;

        // Precompute per-ring envelope + head position
        // Using fixed arrays for up to 4 rings (avoids Vec allocation)
        let mut envelopes = [0.0f32; 4];
        let mut heads = [0.0f32; 4];
        for i in 0..ring_count.min(4) {
            let phase = (t / period + i as f32 / ring_count as f32 - cfg.phase_offset + 1.0) % 1.0;
            let env = (phase * std::f32::consts::PI).sin();
            // Smoothstep: 3x² - 2x³
            envelopes[i] = env * env * (3.0 - 2.0 * env);
            heads[i] = phase * self.reach;
        }

        let amp = cfg.amplitude * intensity;
        let tail_amp = cfg.tail_amplitude * intensity;
        let ring_scale = 1.0 / ring_count as f32;
        let width_param = cfg.ring_width;
        let tail_param = cfg.tail_length;
        let tail_scale = 1.0 / tail_param;

        for (i, cell) in self.cells.iter_mut().enumerate() {
            let dist = self.distances[i];
            let edge = self.edge_falloff[i];

            let mut level = 0.0f32;

            for ri in 0..ring_count.min(4) {
                let head = heads[ri];
                let eased = envelopes[ri];
                let delta = dist - head;
                let abs_delta = delta.abs();

                // Cosine crest — smooth bell centered on the head
                let crest = if abs_delta < width_param {
                    0.5 + 0.5 * (delta / width_param * std::f32::consts::PI).cos()
                } else {
                    0.0
                };

                // Power-law tail — trails behind the head
                let tail = if delta < 0.0 && delta > -tail_param {
                    (1.0 + delta * tail_scale).powf(2.3)
                } else {
                    0.0
                };

                level += (crest * amp + tail * tail_amp) * eased;
            }

            let raw_strength = (level * ring_scale + breath) * edge;
            let strength = raw_strength.min(1.0) * 0.7;

            cell.r = (base_r + delta_r * strength) as u8;
            cell.g = (base_g + delta_g * strength) as u8;
            cell.b = (base_b + delta_b * strength) as u8;
        }

        &self.cells
    }
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substrate_renders_cells() {
        let mut sub = SubstrateState::new(SubstrateConfig::default());
        sub.ensure_geometry(20, 10);
        sub.tick(16, &Store::new());
        let cells = sub.render();
        assert_eq!(cells.len(), 200);
        // Center cells should be different from bg
        assert_ne!(cells[10 * 5 + 5], cells[0]);
    }

    #[test]
    fn substrate_handles_zero_geometry() {
        let mut sub = SubstrateState::new(SubstrateConfig::default());
        sub.tick(16, &Store::new());
        let cells = sub.render();
        assert!(cells.is_empty());
    }

    #[test]
    fn substrate_resize_recomputes() {
        let mut sub = SubstrateState::new(SubstrateConfig::default());
        sub.ensure_geometry(10, 5);
        assert_eq!(sub.distances.len(), 50);
        sub.ensure_geometry(20, 10);
        assert_eq!(sub.distances.len(), 200);
    }

    #[test]
    fn substrate_active_mode_intensifies() {
        let mut store = Store::new();
        let mut thread = crate::store::ThreadModel::new(crate::ids::ThreadId::new(1));
        thread.status = crate::store::ThreadStatus::Running;
        store.threads.insert(crate::ids::ThreadId::new(1), thread);

        let mut sub = SubstrateState::new(SubstrateConfig::default());
        sub.ensure_geometry(20, 10);

        // Tick enough for intensity to ramp up
        for _ in 0..100 {
            sub.tick(16, &store);
        }
        assert_eq!(sub.mode, SubstrateMode::Active);
        assert!(sub.activity_intensity > 0.5);
    }

    #[test]
    fn substrate_period_wraps() {
        let mut sub = SubstrateState::new(SubstrateConfig::default());
        sub.tick(4600, &Store::new());
        assert!(sub.elapsed_ms < 1.0); // Should have wrapped
    }
}
