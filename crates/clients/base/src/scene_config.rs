//! Scene configuration — data-driven, loaded from `scene.toml`.
//!
//! All magic numbers live here. Edit the TOML, save, the TUI hot-reloads.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneConfig {
    #[serde(default)]
    pub animation: AnimationConfig,
    #[serde(default)]
    pub scene: SceneParams,
    #[serde(default)]
    pub camera: CameraConfig,
    #[serde(default)]
    pub colors: ColorConfig,
    #[serde(default)]
    pub fog: FogConfig,
    #[serde(default)]
    pub stars: StarConfig,
    #[serde(default)]
    pub rings: RingsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationConfig {
    #[serde(default = "default_tick_ms")]
    pub tick_ms: u64,
}

fn default_tick_ms() -> u64 {
    66
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            tick_ms: default_tick_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneParams {
    #[serde(default = "default_speed")]
    pub speed: f32,
    #[serde(default, flatten)]
    pub shard: ShardConfig,
    #[serde(default, flatten)]
    pub root: RootConfig,
    #[serde(default, flatten)]
    pub spinner: SpinnerConfig,
    #[serde(default)]
    pub stream: StreamConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardConfig {
    #[serde(default = "default_shard_rot", rename = "shard.rotation_hz")]
    pub shard_rotation_hz: f32,
    #[serde(default = "default_shard_bob_hz", rename = "shard.bob_hz")]
    pub shard_bob_hz: f32,
    #[serde(default = "default_shard_bob_amp", rename = "shard.bob_amplitude")]
    pub shard_bob_amplitude: f32,
    #[serde(default = "default_shard_size", rename = "shard.size")]
    pub shard_size: f32,
}

fn default_speed() -> f32 {
    1.0
}
fn default_shard_rot() -> f32 {
    0.04
}
fn default_shard_bob_hz() -> f32 {
    0.3
}
fn default_shard_bob_amp() -> f32 {
    0.5
}
fn default_shard_size() -> f32 {
    0.015
}

impl Default for SceneParams {
    fn default() -> Self {
        Self {
            speed: default_speed(),
            shard: ShardConfig::default(),
            root: RootConfig::default(),
            spinner: SpinnerConfig::default(),
            stream: StreamConfig::default(),
        }
    }
}

impl Default for ShardConfig {
    fn default() -> Self {
        Self {
            shard_rotation_hz: default_shard_rot(),
            shard_bob_hz: default_shard_bob_hz(),
            shard_bob_amplitude: default_shard_bob_amp(),
            shard_size: default_shard_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootConfig {
    #[serde(default = "default_breathe_hz", rename = "root.breathe_hz")]
    pub breathe_hz: f32,
    #[serde(default = "default_breathe_amp", rename = "root.breathe_amplitude")]
    pub breathe_amplitude: f32,
    #[serde(default = "default_root_rot", rename = "root.rotation_hz")]
    pub rotation_hz: f32,
}

fn default_breathe_hz() -> f32 {
    0.05
}
fn default_breathe_amp() -> f32 {
    0.15
}
fn default_root_rot() -> f32 {
    0.001
}

impl Default for RootConfig {
    fn default() -> Self {
        Self {
            breathe_hz: default_breathe_hz(),
            breathe_amplitude: default_breathe_amp(),
            rotation_hz: default_root_rot(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpinnerConfig {
    #[serde(default = "default_spinner_mult", rename = "spinner.speed_multiplier")]
    pub speed_multiplier: f32,
}

fn default_spinner_mult() -> f32 {
    10.0
}

impl Default for SpinnerConfig {
    fn default() -> Self {
        Self {
            speed_multiplier: default_spinner_mult(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    #[serde(default = "default_stream_speed")]
    pub speed: f32,
}

fn default_stream_speed() -> f32 {
    0.08
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            speed: default_stream_speed(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    #[serde(default)]
    pub theta: f32,
    #[serde(default = "default_phi")]
    pub phi: f32,
    #[serde(default = "default_radius")]
    pub radius: f32,
    #[serde(default = "default_fov")]
    pub fov_degrees: f32,
}

fn default_phi() -> f32 {
    std::f32::consts::PI / 2.2
}
fn default_radius() -> f32 {
    45.0
}
fn default_fov() -> f32 {
    55.0
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            theta: 0.0,
            phi: default_phi(),
            radius: default_radius(),
            fov_degrees: default_fov(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorConfig {
    #[serde(default = "default_bg")]
    pub background: [u8; 3],
    #[serde(default = "default_inner")]
    pub inner_ring: [u8; 3],
    #[serde(default = "default_mid")]
    pub mid_ring: [u8; 3],
    #[serde(default = "default_outer")]
    pub outer_ring: [u8; 3],
    #[serde(default = "default_shard_col")]
    pub shard: [u8; 3],
    #[serde(default = "default_star_col")]
    pub star: [u8; 3],
    #[serde(default = "default_stream_col")]
    pub stream: [u8; 3],
    #[serde(default = "default_frag_col")]
    pub fragment: [u8; 3],
    #[serde(default = "default_fog_col")]
    pub fog: [u8; 3],
}

fn default_bg() -> [u8; 3] {
    [0x1d, 0x20, 0x21]
}
fn default_inner() -> [u8; 3] {
    [0xfe, 0x80, 0x19]
}
fn default_mid() -> [u8; 3] {
    [0x83, 0xa5, 0x98]
}
fn default_outer() -> [u8; 3] {
    [0xd3, 0x86, 0x9b]
}
fn default_shard_col() -> [u8; 3] {
    [0xfe, 0x80, 0x19]
}
fn default_star_col() -> [u8; 3] {
    [0xeb, 0xdb, 0xb2]
}
fn default_stream_col() -> [u8; 3] {
    [0xfa, 0xbd, 0x2f]
}
fn default_frag_col() -> [u8; 3] {
    [0xfe, 0x80, 0x19]
}
fn default_fog_col() -> [u8; 3] {
    [0x25, 0x28, 0x29]
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            background: default_bg(),
            inner_ring: default_inner(),
            mid_ring: default_mid(),
            outer_ring: default_outer(),
            shard: default_shard_col(),
            star: default_star_col(),
            stream: default_stream_col(),
            fragment: default_frag_col(),
            fog: default_fog_col(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FogConfig {
    #[serde(default = "default_fog_strength")]
    pub strength: f32,
}

fn default_fog_strength() -> f32 {
    0.7
}

impl Default for FogConfig {
    fn default() -> Self {
        Self {
            strength: default_fog_strength(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StarConfig {
    #[serde(default = "default_star_count")]
    pub count: usize,
    #[serde(default = "default_star_min_r")]
    pub min_radius: f32,
    #[serde(default = "default_star_max_r")]
    pub max_radius: f32,
    #[serde(default = "default_star_min_s")]
    pub min_size: f32,
    #[serde(default = "default_star_max_s")]
    pub max_size: f32,
    #[serde(default = "default_twinkle_min")]
    pub twinkle_min: f32,
    #[serde(default = "default_twinkle_max")]
    pub twinkle_max: f32,
}

fn default_star_count() -> usize {
    2200
}
fn default_star_min_r() -> f32 {
    60.0
}
fn default_star_max_r() -> f32 {
    380.0
}
fn default_star_min_s() -> f32 {
    0.4
}
fn default_star_max_s() -> f32 {
    2.6
}
fn default_twinkle_min() -> f32 {
    0.3
}
fn default_twinkle_max() -> f32 {
    2.5
}

impl Default for StarConfig {
    fn default() -> Self {
        Self {
            count: default_star_count(),
            min_radius: default_star_min_r(),
            max_radius: default_star_max_r(),
            min_size: default_star_min_s(),
            max_size: default_star_max_s(),
            twinkle_min: default_twinkle_min(),
            twinkle_max: default_twinkle_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingsConfig {
    #[serde(default)]
    pub inner: Vec<RingDef>,
    #[serde(default)]
    pub mid: Vec<RingDef>,
    #[serde(default)]
    pub outer: Vec<RingDef>,
}

impl Default for RingsConfig {
    fn default() -> Self {
        Self {
            inner: vec![
                RingDef {
                    rotation_x: 0.4,
                    rotation_z: 0.3,
                    radius: 4.5,
                    segments: 90,
                    is_polygon: false,
                    speed: 0.024,
                    bob_strength: 1.4,
                },
                RingDef {
                    rotation_x: 1.1,
                    rotation_z: -0.2,
                    radius: 6.0,
                    segments: 60,
                    is_polygon: false,
                    speed: -0.018,
                    bob_strength: 0.8,
                },
                RingDef {
                    rotation_x: 0.8,
                    rotation_z: 0.9,
                    radius: 5.0,
                    segments: 6,
                    is_polygon: true,
                    speed: 0.015,
                    bob_strength: 0.3,
                },
                RingDef {
                    rotation_x: 1.5,
                    rotation_z: 0.1,
                    radius: 7.0,
                    segments: 100,
                    is_polygon: false,
                    speed: -0.012,
                    bob_strength: 1.2,
                },
            ],
            mid: vec![
                RingDef {
                    rotation_x: 0.15,
                    rotation_z: 0.0,
                    radius: 12.0,
                    segments: 110,
                    is_polygon: false,
                    speed: 0.008,
                    bob_strength: 0.5,
                },
                RingDef {
                    rotation_x: 1.45,
                    rotation_z: 0.3,
                    radius: 14.0,
                    segments: 110,
                    is_polygon: false,
                    speed: -0.007,
                    bob_strength: 1.6,
                },
                RingDef {
                    rotation_x: 0.7,
                    rotation_z: -0.6,
                    radius: 11.0,
                    segments: 8,
                    is_polygon: true,
                    speed: 0.006,
                    bob_strength: 0.2,
                },
                RingDef {
                    rotation_x: 1.1,
                    rotation_z: 1.0,
                    radius: 13.0,
                    segments: 110,
                    is_polygon: false,
                    speed: -0.005,
                    bob_strength: 1.1,
                },
            ],
            outer: vec![
                RingDef {
                    rotation_x: 0.2,
                    rotation_z: 0.0,
                    radius: 22.0,
                    segments: 140,
                    is_polygon: false,
                    speed: 0.003,
                    bob_strength: 0.7,
                },
                RingDef {
                    rotation_x: 0.9,
                    rotation_z: 0.5,
                    radius: 28.0,
                    segments: 8,
                    is_polygon: true,
                    speed: -0.0025,
                    bob_strength: 1.3,
                },
                RingDef {
                    rotation_x: 1.4,
                    rotation_z: -0.3,
                    radius: 25.0,
                    segments: 90,
                    is_polygon: false,
                    speed: 0.002,
                    bob_strength: 0.4,
                },
                RingDef {
                    rotation_x: 0.4,
                    rotation_z: 1.2,
                    radius: 35.0,
                    segments: 160,
                    is_polygon: false,
                    speed: -0.0015,
                    bob_strength: 1.0,
                },
                RingDef {
                    rotation_x: 1.2,
                    rotation_z: 0.8,
                    radius: 42.0,
                    segments: 12,
                    is_polygon: true,
                    speed: 0.001,
                    bob_strength: 0.15,
                },
            ],
        }
    }
}

/// A single ring in the orbital scene.
///
/// Each ring is defined by named typed fields rather than a positional array.
/// All fields have sensible defaults so partial definitions are valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingDef {
    #[serde(default)]
    pub rotation_x: f32,
    #[serde(default)]
    pub rotation_z: f32,
    #[serde(default = "default_ring_radius")]
    pub radius: f32,
    #[serde(default = "default_ring_segments")]
    pub segments: usize,
    #[serde(default)]
    pub is_polygon: bool,
    #[serde(default = "default_ring_speed")]
    pub speed: f32,
    #[serde(default = "default_ring_bob")]
    pub bob_strength: f32,
}

fn default_ring_radius() -> f32 {
    10.0
}
fn default_ring_segments() -> usize {
    90
}
fn default_ring_speed() -> f32 {
    0.01
}
fn default_ring_bob() -> f32 {
    1.0
}

impl Default for SceneConfig {
    fn default() -> Self {
        Self {
            animation: AnimationConfig::default(),
            scene: SceneParams::default(),
            camera: CameraConfig::default(),
            colors: ColorConfig::default(),
            fog: FogConfig::default(),
            stars: StarConfig::default(),
            rings: RingsConfig::default(),
        }
    }
}

impl SceneConfig {
    /// Load from a TOML file. Returns defaults if file missing or invalid.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                eprintln!(
                    "warn: failed to parse {}: {}, using defaults",
                    path.display(),
                    e
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Find config file: check CWD, then ~/.config/ryeos/scene.toml
    pub fn find() -> Self {
        let candidates: Vec<PathBuf> = vec![PathBuf::from("scene.toml"), dirs().join("scene.toml")];
        for path in &candidates {
            if path.exists() {
                return Self::load(path);
            }
        }
        Self::default()
    }

    /// Check if the config file has been modified (for hot-reload).
    /// Returns (exists, mtime) or None if can't stat.
    pub fn mtime(path: &Path) -> Option<std::time::SystemTime> {
        std::fs::metadata(path).ok()?.modified().ok()
    }
}

fn dirs() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
        });
    base.join("ryeos")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_loads() {
        let config = SceneConfig::default();
        assert_eq!(config.animation.tick_ms, 66);
        assert_eq!(config.camera.radius, 45.0);
        assert_eq!(config.rings.inner.len(), 4);
        assert_eq!(config.rings.mid.len(), 4);
        assert_eq!(config.rings.outer.len(), 5);
        assert_eq!(config.stars.count, 2200);
    }

    #[test]
    fn roundtrip_toml() {
        let config = SceneConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: SceneConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.animation.tick_ms, 66);
        assert_eq!(parsed.rings.inner.len(), 4);
    }

    #[test]
    fn mixed_int_float_named_fields() {
        // Verify named fields parse correctly with partial specification
        let toml_str = r#"
[[rings.inner]]
rotation_x = 0.4
rotation_z = 0.3
radius = 4.5
segments = 90
speed = 0.024
bob_strength = 1.4
"#;
        let config: SceneConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.rings.inner.len(), 1);
        assert_eq!(config.rings.inner[0].segments, 90);
        assert!(!config.rings.inner[0].is_polygon);
    }
}
