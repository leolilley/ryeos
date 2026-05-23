//! 3D math utilities for the animated substrate.
//!
//! Simple Vec3, rotation, and perspective projection for generating
//! scene primitives from the Three.js-inspired 3D scene.

use crate::scene::Vec2;

/// 3D vector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn zero() -> Self {
        Self { x: 0.0, y: 0.0, z: 0.0 }
    }

    pub fn length(&self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    pub fn normalize(&self) -> Self {
        let len = self.length();
        if len > 0.0 {
            Self {
                x: self.x / len,
                y: self.y / len,
                z: self.z / len,
            }
        } else {
            *self
        }
    }
}

impl std::ops::Add for Vec3 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
            z: self.z + rhs.z,
        }
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
            z: self.z - rhs.z,
        }
    }
}

impl std::ops::Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, s: f32) -> Self {
        Self {
            x: self.x * s,
            y: self.y * s,
            z: self.z * s,
        }
    }
}

/// Rotate a point around the X axis.
pub fn rotate_x(p: &Vec3, angle: f32) -> Vec3 {
    let c = angle.cos();
    let s = angle.sin();
    Vec3::new(p.x, p.y * c - p.z * s, p.y * s + p.z * c)
}

/// Rotate a point around the Z axis.
pub fn rotate_z(p: &Vec3, angle: f32) -> Vec3 {
    let c = angle.cos();
    let s = angle.sin();
    Vec3::new(p.x * c - p.y * s, p.x * s + p.y * c, p.z)
}

/// Rotate a point around the Y axis.
pub fn rotate_y(p: &Vec3, angle: f32) -> Vec3 {
    let c = angle.cos();
    let s = angle.sin();
    Vec3::new(p.x * c + p.z * s, p.y, -p.x * s + p.z * c)
}

/// Camera state — spherical coordinates for orbit.
#[derive(Debug, Clone)]
pub struct Camera3D {
    pub theta: f32,
    pub phi: f32,
    pub radius: f32,
    pub target: Vec3,
    pub offset: Vec3,
}

impl Default for Camera3D {
    fn default() -> Self {
        Self {
            theta: 0.0,
            phi: std::f32::consts::PI / 2.2,
            radius: 45.0,
            target: Vec3::zero(),
            offset: Vec3::zero(),
        }
    }
}

impl Camera3D {
    pub fn position(&self) -> Vec3 {
        Vec3::new(
            self.radius * self.phi.sin() * self.theta.sin() + self.offset.x,
            self.radius * self.phi.cos() + self.offset.y,
            self.radius * self.phi.sin() * self.theta.cos() + self.offset.z,
        )
    }

    /// Project a 3D world point to normalized screen coordinates [0, 1].
    /// Simple perspective projection looking at the target.
    pub fn project(&self, point: &Vec3) -> Option<Vec2> {
        let cam_pos = self.position();
        let forward = (self.target - cam_pos).normalize();
        let world_up = Vec3::new(0.0, 1.0, 0.0);
        let right = Vec3::new(
            forward.z * world_up.y - forward.y * world_up.z,
            forward.x * world_up.z - forward.z * world_up.x,
            forward.y * world_up.x - forward.x * world_up.y,
        )
        .normalize();
        let up = Vec3::new(
            right.y * forward.z - right.z * forward.y,
            right.z * forward.x - right.x * forward.z,
            right.x * forward.y - right.y * forward.x,
        );

        let rel = *point - cam_pos;
        let depth = rel.x * forward.x + rel.y * forward.y + rel.z * forward.z;
        if depth <= 0.1 {
            return None; // Behind camera
        }

        let screen_x = rel.x * right.x + rel.y * right.y + rel.z * right.z;
        let screen_y = rel.x * up.x + rel.y * up.y + rel.z * up.z;

        let fov = 55.0_f32.to_radians();
        let aspect = 2.0; // Approximate terminal aspect ratio
        let scale = (fov * 0.5).tan();

        Some(Vec2::new(
            0.5 + screen_x / (depth * scale * aspect),
            0.5 - screen_y / (depth * scale), // Flip Y for screen coords
        ))
    }
}

/// Generate circle points in the XZ plane.
pub fn circle_points(r: f32, segments: usize) -> Vec<Vec3> {
    (0..=segments)
        .map(|i| {
            let a = (i as f32 / segments as f32) * std::f32::consts::TAU;
            Vec3::new(a.cos() * r, 0.0, a.sin() * r)
        })
        .collect()
}

/// Generate polygon points in the XZ plane.
pub fn polygon_points(sides: usize, r: f32) -> Vec<Vec3> {
    circle_points(r, sides)
}

/// Orbital spinner — a ring that spins around an axis.
#[derive(Debug, Clone)]
pub struct Spinner {
    pub rotation_x: f32,
    pub rotation_z: f32,
    pub spin_speed: f32,
    pub bob_strength: f32,
    pub current_angle: f32,
    pub radius: f32,
    pub segments: usize,
    pub is_polygon: bool,
}

/// Floating fragment — a small crystal orbiting in a band.
#[derive(Debug, Clone)]
pub struct Fragment {
    pub orbit_angle: f32,
    pub orbit_radius: f32,
    pub orbit_height: f32,
    pub phase: f32,
    pub scale: f32,
    pub speed: f32,
    pub tumble_x: f32,
    pub tumble_y: f32,
    pub tumble_z: f32,
}

/// Particle stream — converging particles creating a focal disc.
#[derive(Debug, Clone)]
pub struct ParticleStream {
    pub count: usize,
    pub phases: Vec<f32>,
    pub tilt_x: f32,
    pub tilt_z: f32,
    pub spin: f32,
}

/// Star in the background starfield.
#[derive(Debug, Clone)]
pub struct Star {
    pub pos: Vec3,
    pub base_size: f32,
    pub size: f32,
    pub freq: f32,
    pub phase: f32,
}

/// Full 3D scene state matching the Three.js original.
#[derive(Debug, Clone)]
pub struct Scene3D {
    pub camera: Camera3D,

    // Shard — central crystal
    pub shard_rotation_y: f32,
    pub shard_bob_y: f32,

    // Orbital rings — inner, mid, outer
    pub spinners: Vec<Spinner>,

    // Floating fragments
    pub fragments: Vec<Fragment>,

    // Energy streams (focal disc)
    pub streams: Vec<ParticleStream>,

    // Starfield
    pub stars: Vec<Star>,

    // System-level animation
    pub root_y: f32,
    pub root_rotation_y: f32,

    // Time tracking
    pub time: f32,
}

impl Default for Scene3D {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene3D {
    /// Create scene from data-driven config.
    pub fn from_config(cfg: &crate::scene_config::SceneConfig) -> Self {
        let mut scene = Self {
            camera: Camera3D {
                theta: cfg.camera.theta,
                phi: cfg.camera.phi,
                radius: cfg.camera.radius,
                target: Vec3::zero(),
                offset: Vec3::zero(),
            },
            shard_rotation_y: 0.0,
            shard_bob_y: 0.0,
            spinners: Vec::new(),
            fragments: Vec::new(),
            streams: Vec::new(),
            stars: Vec::new(),
            root_y: 0.0,
            root_rotation_y: 0.0,
            time: 0.0,
        };

        // Build rings from config
        for ring in &cfg.rings.inner {
            scene.spinners.push(Spinner {
                rotation_x: ring.rotation_x,
                rotation_z: ring.rotation_z,
                spin_speed: ring.speed,
                bob_strength: ring.bob_strength,
                current_angle: 0.0,
                radius: ring.radius,
                segments: ring.segments,
                is_polygon: ring.is_polygon,
            });
        }
        for ring in &cfg.rings.mid {
            scene.spinners.push(Spinner {
                rotation_x: ring.rotation_x,
                rotation_z: ring.rotation_z,
                spin_speed: ring.speed,
                bob_strength: ring.bob_strength,
                current_angle: 0.0,
                radius: ring.radius,
                segments: ring.segments,
                is_polygon: ring.is_polygon,
            });
            // Dashed companion
            scene.spinners.push(Spinner {
                rotation_x: ring.rotation_x + 0.06,
                rotation_z: ring.rotation_z - 0.04,
                spin_speed: ring.speed * -0.6,
                bob_strength: ring.bob_strength * 0.7,
                current_angle: 0.0,
                radius: ring.radius * 1.08,
                segments: 70,
                is_polygon: false,
            });
        }
        for ring in &cfg.rings.outer {
            scene.spinners.push(Spinner {
                rotation_x: ring.rotation_x,
                rotation_z: ring.rotation_z,
                spin_speed: ring.speed,
                bob_strength: ring.bob_strength,
                current_angle: 0.0,
                radius: ring.radius,
                segments: ring.segments,
                is_polygon: ring.is_polygon,
            });
        }

        // Fragments (keep hardcoded positions — 18 is plenty)
        let frag_orbits = [
            (0.0, 5.0, 0.8, 3.0, 0.25, 0.018),
            (1.05, 5.5, -0.4, 2.8, 0.18, 0.020),
            (2.09, 4.8, 0.3, 4.2, 0.30, 0.016),
            (3.0, 5.2, -0.7, 0.8, 0.20, 0.022),
            (4.19, 4.5, 0.5, 5.5, 0.15, 0.019),
            (5.24, 5.8, -0.2, 4.0, 0.22, 0.017),
            (0.5, 9.0, 1.5, 2.5, 0.40, 0.012),
            (1.76, 10.5, -1.2, 3.8, 0.35, 0.010),
            (3.01, 8.5, 0.5, 3.0, 0.50, 0.011),
            (4.27, 11.0, -2.0, 0.5, 0.30, 0.013),
            (5.53, 9.5, 1.0, 4.0, 0.45, 0.009),
            (0.9, 10.0, -0.3, 4.0, 0.28, 0.014),
            (0.3, 18.0, 2.5, 0.7, 0.60, 0.005),
            (1.5, 22.0, -1.8, 2.0, 0.55, 0.004),
            (2.8, 16.0, 0.8, 3.5, 0.70, 0.006),
            (4.0, 20.0, -3.0, 1.2, 0.45, 0.003),
            (5.2, 24.0, 1.5, 4.5, 0.35, 0.004),
            (3.6, 15.0, -0.5, 2.8, 0.50, 0.007),
        ];
        for (angle, r, h, phase, scale, speed) in &frag_orbits {
            scene.fragments.push(Fragment {
                orbit_angle: *angle,
                orbit_radius: *r,
                orbit_height: *h,
                phase: *phase,
                scale: *scale,
                speed: *speed,
                tumble_x: (rand_simple() - 0.5) * 0.012,
                tumble_y: (rand_simple() - 0.5) * 0.016,
                tumble_z: (rand_simple() - 0.5) * 0.010,
            });
        }

        // Particle streams (4 streams, configurable count)
        let stream_defs = [
            (0.3, 0.2, 1.0),
            (1.2, -0.5, -0.8),
            (0.7, 0.9, 0.6),
            (-0.4, 1.3, -1.1),
        ];
        for (tilt_x, tilt_z, spin) in &stream_defs {
            let count = 120;
            let phases: Vec<f32> = (0..count).map(|_| rand_simple()).collect();
            scene.streams.push(ParticleStream {
                count,
                phases,
                tilt_x: *tilt_x,
                tilt_z: *tilt_z,
                spin: *spin,
            });
        }

        // Stars (configurable count)
        for _ in 0..cfg.stars.count {
            let theta = rand_simple() * std::f32::consts::TAU;
            let phi = (2.0 * rand_simple() - 1.0).acos();
            let r = cfg.stars.min_radius + rand_simple() * (cfg.stars.max_radius - cfg.stars.min_radius);
            scene.stars.push(Star {
                pos: Vec3::new(
                    r * phi.sin() * theta.cos(),
                    r * phi.sin() * theta.sin(),
                    r * phi.cos(),
                ),
                base_size: cfg.stars.min_size + rand_simple() * (cfg.stars.max_size - cfg.stars.min_size),
                size: cfg.stars.min_size + rand_simple() * (cfg.stars.max_size - cfg.stars.min_size),
                freq: cfg.stars.twinkle_min + rand_simple() * (cfg.stars.twinkle_max - cfg.stars.twinkle_min),
                phase: rand_simple() * std::f32::consts::TAU,
            });
        }

        scene
    }

    /// Create the default scene (hardcoded, same as before).
    pub fn new() -> Self {
        Self::from_config(&crate::scene_config::SceneConfig::default())
    }

    /// Advance the scene by dt seconds.
    pub fn tick(&mut self, dt: f32) {
        self.time += dt;
        let t = self.time;

        // Shard rotation and bob
        self.shard_rotation_y = t * 0.04;
        self.shard_bob_y = (t * 0.3).sin() * 0.5;

        // Spinners
        for spinner in &mut self.spinners {
            spinner.current_angle += spinner.spin_speed * dt * 10.0;
        }

        // Star twinkle
        for star in &mut self.stars {
            let flicker = 0.55 + 0.45 * (t * star.freq + star.phase).sin();
            star.size = star.base_size * flicker;
        }

        // Stream particles — advance phases
        let speed = 0.08;
        for stream in &mut self.streams {
            for phase in &mut stream.phases {
                *phase = (*phase + dt * speed) % 1.0;
            }
        }

        // System breathe
        self.root_y = (t * 0.05).sin() * 0.15;
        self.root_rotation_y = t * 0.001;
    }

    /// Tick with data-driven config parameters.
    pub fn tick_with_config(&mut self, dt: f32, cfg: Option<&crate::scene_config::SceneConfig>) {
        let c = match cfg {
            Some(c) => c,
            None => return self.tick(dt),
        };

        self.time += dt;
        let t = self.time;

        // Shard rotation and bob (configurable)
        self.shard_rotation_y = t * c.scene.shard.shard_rotation_hz;
        self.shard_bob_y = (t * c.scene.shard.shard_bob_hz).sin() * c.scene.shard.shard_bob_amplitude;

        // Spinners (configurable multiplier)
        let mult = c.scene.spinner.speed_multiplier;
        for spinner in &mut self.spinners {
            spinner.current_angle += spinner.spin_speed * dt * mult;
        }

        // Star twinkle
        for star in &mut self.stars {
            let flicker = 0.55 + 0.45 * (t * star.freq + star.phase).sin();
            star.size = star.base_size * flicker;
        }

        // Stream particles (configurable speed)
        let speed = c.scene.stream.speed;
        for stream in &mut self.streams {
            for phase in &mut stream.phases {
                *phase = (*phase + dt * speed) % 1.0;
            }
        }

        // System breathe (configurable)
        self.root_y = (t * c.scene.root.breathe_hz).sin() * c.scene.root.breathe_amplitude;
        self.root_rotation_y = t * c.scene.root.rotation_hz;
    }
}

/// Simple deterministic random for initialization (we don't need true randomness).
fn rand_simple() -> f32 {
    use std::cell::Cell;
    thread_local! {
        static SEED: Cell<u64> = const { Cell::new(12345) };
    }
    SEED.with(|s| {
        let mut seed = s.get();
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        s.set(seed);
        (seed >> 33) as f32 / (1u64 << 31) as f32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec3_basic_ops() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);
        let sum = a + b;
        assert_eq!(sum.x, 5.0);
        assert_eq!(sum.y, 7.0);
        assert_eq!(sum.z, 9.0);

        let scaled = a * 2.0;
        assert_eq!(scaled.x, 2.0);
    }

    #[test]
    fn rotation_preserves_distance() {
        let p = Vec3::new(3.0, 4.0, 0.0);
        let r = rotate_y(&p, 1.0);
        assert!((p.length() - r.length()).abs() < 0.001);
    }

    #[test]
    fn scene3d_initializes() {
        let scene = Scene3D::new();
        assert_eq!(scene.spinners.len(), 17); // 4 inner + 8 mid + 5 outer
        assert_eq!(scene.fragments.len(), 18);
        assert_eq!(scene.streams.len(), 4);
        assert_eq!(scene.stars.len(), 2200);
    }

    #[test]
    fn camera_projection() {
        let cam = Camera3D::default();
        let origin = Vec3::zero();
        let proj = cam.project(&origin);
        assert!(proj.is_some());
        let p = proj.unwrap();
        assert!(p.x > 0.0 && p.x < 1.0);
        assert!(p.y > 0.0 && p.y < 1.0);
    }
}
