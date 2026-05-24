//! Animation state and 3D scene primitive generation.
//!
//! Core owns the Scene3D state. Terminal rasterizes to Braille/block cells;
//! web renders to Canvas 2D. Both consume the same ScenePrimitive list.

use crate::math3d::{
    rotate_x, rotate_y, rotate_z, Scene3D, Vec3,
};
use crate::scene::{Rgb, ScenePrimitive, Vec2};
use crate::scene_config::SceneConfig;
use crate::store::Store;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubstrateMode {
    Idle,
    Active,
    ErrorPulse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationState {
    pub time_ms: u64,
    pub mode: SubstrateMode,
    pub activity_intensity: f32,
    /// Frames since last config hot-reload check.
    #[serde(skip)]
    reload_counter: u32,
    #[serde(skip)]
    scene: Option<Scene3D>,
    #[serde(skip)]
    config: Option<SceneConfig>,
    #[serde(skip)]
    config_mtime: Option<std::time::SystemTime>,
}

impl Default for AnimationState {
    fn default() -> Self {
        Self {
            time_ms: 0,
            mode: SubstrateMode::Idle,
            activity_intensity: 0.0,
            reload_counter: 0,
            scene: None,
            config: None,
            config_mtime: None,
        }
    }
}

impl AnimationState {
    /// Set the scene configuration (loaded from scene.toml).
    pub fn set_config(&mut self, config: SceneConfig) {
        // If scene already exists, recreate it with new config
        self.scene = None; // will rebuild on next tick
        self.config = Some(config);
    }

    /// Hot-reload config from scene.toml if the file has been modified.
    fn check_config_reload(&mut self) {
        let path = std::path::PathBuf::from("scene.toml");
        if !path.exists() { return; }

        let mtime = match SceneConfig::mtime(&path) {
            Some(m) => m,
            None => return,
        };

        // Only reload if mtime changed since last load
        let should_reload = self.config_mtime.map_or(true, |prev| mtime != prev);
        if should_reload {
            let new_config = SceneConfig::load(&path);
            self.config = Some(new_config);
            self.config_mtime = Some(mtime);
            // Force scene rebuild on next tick
            self.scene = None;
        }
    }
    fn ensure_scene(&mut self) {
        if self.scene.is_none() {
            self.scene = Some(Scene3D::from_config(
                self.config.as_ref().unwrap_or(&SceneConfig::default()),
            ));
        }
    }

    /// Advance animation by delta_ms and update mode from store state.
    pub fn tick(&mut self, delta_ms: u64, store: &Store) {
        self.time_ms += delta_ms;

        // Hot-reload config every ~30 frames (~2 seconds at 66ms tick)
        self.reload_counter += 1;
        if self.reload_counter >= 30 {
            self.reload_counter = 0;
            self.check_config_reload();
        }

        self.ensure_scene();

        let speed = self.config.as_ref().map(|c| c.scene.speed).unwrap_or(1.0);
        let dt = (delta_ms as f32 / 1000.0) * speed;

        if let Some(ref mut scene) = self.scene {
            scene.tick_with_config(dt, self.config.as_ref());
        }

        // Determine mode from store
        let running = store.running_thread_count();
        let has_errors = store
            .trust_alerts
            .iter()
            .any(|a| matches!(a.severity, crate::store::TrustSeverity::Error));

        self.mode = if has_errors {
            SubstrateMode::ErrorPulse
        } else if running > 0 {
            SubstrateMode::Active
        } else {
            SubstrateMode::Idle
        };

        let target_intensity = match self.mode {
            SubstrateMode::Idle => 0.15,
            SubstrateMode::Active => 0.6,
            SubstrateMode::ErrorPulse => 0.85,
        };
        self.activity_intensity += (target_intensity - self.activity_intensity) * 0.05;
    }

    /// Generate scene primitives by projecting the 3D scene to 2D.
    pub fn generate_primitives(&self) -> Vec<ScenePrimitive> {
        let scene = match &self.scene {
            Some(s) => s,
            None => return Vec::new(),
        };

        let t = scene.time;
        let mut prims = Vec::new();
        let intensity = self.activity_intensity;

        // --- Colors from config ---
        let cfg = self.config.as_ref();
        let shard_color = cfg.map(|c| Rgb::new(c.colors.shard[0], c.colors.shard[1], c.colors.shard[2])).unwrap_or(Rgb::new(0xfe, 0x80, 0x19));
        let inner_color = cfg.map(|c| Rgb::new(c.colors.inner_ring[0], c.colors.inner_ring[1], c.colors.inner_ring[2])).unwrap_or(Rgb::new(0xfe, 0x80, 0x19));
        let mid_color = cfg.map(|c| Rgb::new(c.colors.mid_ring[0], c.colors.mid_ring[1], c.colors.mid_ring[2])).unwrap_or(Rgb::new(0x83, 0xa5, 0x98));
        let outer_color = cfg.map(|c| Rgb::new(c.colors.outer_ring[0], c.colors.outer_ring[1], c.colors.outer_ring[2])).unwrap_or(Rgb::new(0xd3, 0x86, 0x9b));
        let star_color = cfg.map(|c| Rgb::new(c.colors.star[0], c.colors.star[1], c.colors.star[2])).unwrap_or(Rgb::new(0xeb, 0xdb, 0xb2));
        let stream_color = cfg.map(|c| Rgb::new(c.colors.stream[0], c.colors.stream[1], c.colors.stream[2])).unwrap_or(Rgb::new(0xfa, 0xbd, 0x2f));
        let frag_color = cfg.map(|c| Rgb::new(c.colors.fragment[0], c.colors.fragment[1], c.colors.fragment[2])).unwrap_or(Rgb::new(0xfe, 0x80, 0x19));

        // --- Stars ---
        for star in &scene.stars {
            let pos2d = scene.camera.project(&star.pos);
            if let Some(p) = pos2d {
                if p.x >= 0.0 && p.x <= 1.0 && p.y >= 0.0 && p.y <= 1.0 {
                    prims.push(ScenePrimitive::Point {
                        pos: p,
                        z: 0.0,
                        color: star_color,
                        size: star.size * 0.002,
                        opacity: 0.15 + intensity * 0.1,
                    });
                }
            }
        }

        // --- Orbital rings (spinners) ---
        // Determine color band by radius
        let get_ring_color = |radius: f32| -> Rgb {
            if radius < 8.0 {
                inner_color
            } else if radius < 16.0 {
                mid_color
            } else {
                outer_color
            }
        };

        let get_ring_opacity = |radius: f32, intensity: f32| -> f32 {
            let base = if radius < 8.0 {
                0.35
            } else if radius < 16.0 {
                0.22
            } else {
                0.12
            };
            base + intensity * 0.15
        };

        for spinner in &scene.spinners {
            let color = get_ring_color(spinner.radius);
            let opacity = get_ring_opacity(spinner.radius, intensity);
            let segments = spinner.segments.min(64); // cap for perf

            let points_3d: Vec<Vec3> = if spinner.is_polygon {
                let n = segments.min(8);
                (0..=n)
                    .map(|i| {
                        let a = (i as f32 / n as f32) * std::f32::consts::TAU;
                        let mut p = Vec3::new(a.cos() * spinner.radius, 0.0, a.sin() * spinner.radius);
                        p = rotate_x(&p, spinner.rotation_x);
                        p = rotate_z(&p, spinner.rotation_z);
                        p = rotate_y(&p, spinner.current_angle);
                        p.y += spinner.bob_strength * (t * 2.0 + spinner.current_angle).sin();
                        p = rotate_y(&p, scene.root_rotation_y);
                        p.y += scene.root_y;
                        p
                    })
                    .collect()
            } else {
                (0..=segments)
                    .map(|i| {
                        let a = (i as f32 / segments as f32) * std::f32::consts::TAU;
                        let mut p = Vec3::new(a.cos() * spinner.radius, 0.0, a.sin() * spinner.radius);
                        p = rotate_x(&p, spinner.rotation_x);
                        p = rotate_z(&p, spinner.rotation_z);
                        p = rotate_y(&p, spinner.current_angle);
                        p.y += spinner.bob_strength * (t * 2.0 + spinner.current_angle).sin();
                        p = rotate_y(&p, scene.root_rotation_y);
                        p.y += scene.root_y;
                        p
                    })
                    .collect()
            };

            // Project to 2D
            let points_2d: Vec<Option<Vec2>> = points_3d.iter().map(|p| scene.camera.project(p)).collect();

            if spinner.is_polygon {
                // Draw as a closed polygon
                let verts: Vec<Vec2> = points_2d.iter().filter_map(|&p| p).collect();
                if verts.len() >= 3 {
                    // Average z for depth
                    prims.push(ScenePrimitive::Polygon {
                        vertices: verts,
                        z: 0.5,
                        color,
                        opacity,
                    });
                }
            } else {
                // Draw as line segments
                for i in 0..points_2d.len().saturating_sub(1) {
                    match (points_2d[i], points_2d[i + 1]) {
                        (Some(from), Some(to)) => {
                            prims.push(ScenePrimitive::Line {
                                from,
                                to,
                                z: 0.5,
                                color,
                                thickness: if spinner.radius < 8.0 { 1.2 } else { 0.8 },
                                opacity,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }

        // --- Central shard ---
        let shard_pos = Vec3::new(0.0, scene.shard_bob_y, 0.0);
        if let Some(p) = scene.camera.project(&shard_pos) {
            prims.push(ScenePrimitive::Point {
                pos: p,
                z: 1.0,
                color: shard_color,
                size: 0.015 + intensity * 0.005,
                opacity: 0.8 + intensity * 0.2,
            });
        }

        // --- Floating fragments ---
        for frag in &scene.fragments {
            let angle = frag.orbit_angle + t * frag.speed * 10.0;
            let mut pos = Vec3::new(
                angle.cos() * frag.orbit_radius,
                frag.orbit_height + (t * 0.5 + frag.phase).sin() * 0.8,
                angle.sin() * frag.orbit_radius,
            );
            pos = rotate_y(&pos, scene.root_rotation_y);
            pos.y += scene.root_y;

            if let Some(p) = scene.camera.project(&pos) {
                prims.push(ScenePrimitive::Point {
                    pos: p,
                    z: 0.7,
                    color: frag_color,
                    size: frag.scale * 0.006,
                    opacity: 0.4 + intensity * 0.2,
                });
            }
        }

        // --- Particle streams ---
        for stream in &scene.streams {
            let tilt_x = stream.tilt_x;
            let tilt_z = stream.tilt_z;
            let spin = stream.spin;

            // Sample a subset of particles for rendering (cap at 40 per stream for perf)
            let step = (stream.count / 40).max(1);
            for (_i, phase) in stream.phases.iter().enumerate().step_by(step) {
                let p = *phase;
                let r = p * 15.0; // converge to center
                let angle = p * std::f32::consts::TAU * 3.0 + t * spin * 2.0;

                let mut pos = Vec3::new(
                    angle.cos() * r,
                    ((1.0 - p) * 8.0 - 4.0).sin() * 0.5,
                    angle.sin() * r,
                );
                pos = rotate_x(&pos, tilt_x);
                pos = rotate_z(&pos, tilt_z);
                pos = rotate_y(&pos, scene.root_rotation_y);
                pos.y += scene.root_y;

                if let Some(proj) = scene.camera.project(&pos) {
                    prims.push(ScenePrimitive::Point {
                        pos: proj,
                        z: 0.6,
                        color: stream_color,
                        size: 0.002,
                        opacity: (1.0 - p) * 0.3 * (0.5 + intensity),
                    });
                }
            }
        }

        // --- Error pulse ---
        if matches!(self.mode, SubstrateMode::ErrorPulse) {
            let pulse = (t * 4.0).sin() * 0.5 + 0.5;
            prims.push(ScenePrimitive::Ring {
                center: Vec2::new(0.5, 0.5),
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
    fn animation_generates_primitives() {
        let mut anim = AnimationState::default();
        for _ in 0..120 {
            anim.tick(16, &Store::new());
        }
        let prims = anim.generate_primitives();
        let points = prims.iter().filter(|p| matches!(p, ScenePrimitive::Point { .. })).count();
        let lines = prims.iter().filter(|p| matches!(p, ScenePrimitive::Line { .. })).count();
        let polys = prims.iter().filter(|p| matches!(p, ScenePrimitive::Polygon { .. })).count();
        eprintln!("Primitives: total={} points={} lines={} polys={}", prims.len(), points, lines, polys);
        assert!(!prims.is_empty(), "scene should produce primitives");

        // Verify JSON roundtrip works (web uses serde_json)
        let json = serde_json::to_string(&prims).unwrap();
        eprintln!("JSON length: {} bytes", json.len());
        eprintln!("First 300 chars: {}", &json[..json.len().min(300)]);
        assert!(json.len() > 100, "JSON should not be empty");
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
    fn animation_ticks_advance_scene() {
        let mut anim = AnimationState::default();
        anim.tick(100, &Store::new());
        anim.tick(100, &Store::new());
        assert!(anim.scene.is_some());
        assert!(anim.scene.as_ref().unwrap().time > 0.0);
    }
}
