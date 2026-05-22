//! Animation state and substrate primitive generation.
//!
//! Core owns animation state. Terminal rasterizes to Braille/block cells;
//! web renders to Canvas 2D.

use crate::scene::{Rgb, ScenePrimitive, Vec2};
use crate::store::Store;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubstrateMode {
    Idle,
    Active,
    ErrorPulse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub life: f32,
    pub max_life: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationState {
    pub time_ms: u64,
    pub mode: SubstrateMode,
    pub activity_intensity: f32,
    pub particles: Vec<Particle>,
}

impl Default for AnimationState {
    fn default() -> Self {
        Self {
            time_ms: 0,
            mode: SubstrateMode::Idle,
            activity_intensity: 0.0,
            particles: Vec::new(),
        }
    }
}

impl AnimationState {
    /// Advance animation by delta_ms and update mode from store state.
    pub fn tick(&mut self, delta_ms: u64, store: &Store) {
        self.time_ms += delta_ms;

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

        // Smooth mode transition
        if self.mode != target_mode {
            self.mode = target_mode;
        }

        // Update activity intensity
        let target_intensity = match self.mode {
            SubstrateMode::Idle => 0.15,
            SubstrateMode::Active => 0.6,
            SubstrateMode::ErrorPulse => 0.85,
        };
        self.activity_intensity += (target_intensity - self.activity_intensity) * 0.05;
    }

    /// Generate scene primitives for the substrate background.
    pub fn generate_primitives(&self) -> Vec<ScenePrimitive> {
        let t = self.time_ms as f32 / 1000.0;
        let mut prims = Vec::new();

        let shard_color = Rgb::new(0xfe, 0x80, 0x19); // orange
        let inner_color = Rgb::new(0xfe, 0x80, 0x19); // orange/yellow
        let mid_color = Rgb::new(0x83, 0xa5, 0x98); // blue
        let outer_color = Rgb::new(0xd3, 0x86, 0x9b); // purple

        let center = Vec2::new(0.5, 0.5);
        let intensity = self.activity_intensity;

        // Central shard point
        let shard_bob = (t * 0.3).sin() * 0.01;
        prims.push(ScenePrimitive::Point {
            pos: Vec2::new(0.5, 0.5 + shard_bob),
            z: 1.0,
            color: shard_color,
            size: 0.012 + intensity * 0.004,
            opacity: 0.7 + intensity * 0.3,
        });

        // Inner orbital rings (2)
        for i in 0..2 {
            let radius = 0.08 + i as f32 * 0.04;
            let speed = 0.4 + i as f32 * 0.15;
            let rotation = t * speed;
            let tilt = 0.3 + i as f32 * 0.2;
            prims.push(ScenePrimitive::Ring {
                center,
                radius,
                tilt,
                rotation,
                color: inner_color,
                opacity: 0.3 + intensity * 0.2,
            });
        }

        // Mid orbital rings (2)
        for i in 0..2 {
            let radius = 0.18 + i as f32 * 0.05;
            let speed = 0.2 + i as f32 * 0.1;
            let rotation = t * speed + 1.0;
            let tilt = 0.5 + i as f32 * 0.15;
            prims.push(ScenePrimitive::Ring {
                center,
                radius,
                tilt,
                rotation,
                color: mid_color,
                opacity: 0.2 + intensity * 0.15,
            });
        }

        // Outer ring (1)
        prims.push(ScenePrimitive::Ring {
            center,
            radius: 0.35,
            tilt: 0.6,
            rotation: t * 0.1 + 2.0,
            color: outer_color,
            opacity: 0.12 + intensity * 0.1,
        });

        // Scatter some particles
        let particle_count = (5.0 + intensity * 8.0) as usize;
        for i in 0..particle_count.min(12) {
            let angle = (i as f32 * 2.39996) + t * 0.3; // golden angle + rotation
            let dist = 0.1 + (i as f32 * 0.03) % 0.3;
            let px = 0.5 + angle.cos() * dist;
            let py = 0.5 + angle.sin() * dist * 0.5; // squish for perspective
            prims.push(ScenePrimitive::Point {
                pos: Vec2::new(px, py),
                z: 0.5,
                color: shard_color,
                size: 0.003,
                opacity: 0.3 * intensity,
            });
        }

        // Error pulse: red tinted ring
        if matches!(self.mode, SubstrateMode::ErrorPulse) {
            let pulse = (t * 4.0).sin() * 0.5 + 0.5;
            prims.push(ScenePrimitive::Ring {
                center,
                radius: 0.15 + pulse * 0.1,
                tilt: 0.4,
                rotation: t * 0.5,
                color: Rgb::new(0xfb, 0x49, 0x34),
                opacity: 0.3 + pulse * 0.3,
            });
        }

        prims
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animation_is_deterministic_for_same_time() {
        let mut anim = AnimationState::default();
        let store = Store::new();
        anim.tick(100, &store);
        anim.tick(100, &store);

        let p1 = anim.generate_primitives();

        let mut anim2 = AnimationState::default();
        anim2.tick(100, &store);
        anim2.tick(100, &store);

        let p2 = anim2.generate_primitives();
        assert_eq!(p1, p2);
    }

    #[test]
    fn animation_active_mode_when_thread_running() {
        let mut store = Store::new();
        let mut thread = crate::store::ThreadModel::new(crate::ids::ThreadId::new(1));
        thread.status = crate::store::ThreadStatus::Running;
        store.threads.insert(crate::ids::ThreadId::new(1), thread);

        let mut anim = AnimationState::default();
        anim.tick(100, &store);
        assert_eq!(anim.mode, SubstrateMode::Active);
    }

    #[test]
    fn animation_generates_primitives() {
        let anim = AnimationState::default();
        let prims = anim.generate_primitives();
        assert!(!prims.is_empty());
    }
}
