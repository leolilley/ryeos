//! Scene primitives for the animated substrate.
//!
//! Shared between terminal (Braille/block raster) and web (Canvas 2D).

use serde::{Deserialize, Serialize};

/// 2D floating-point position in normalized [0, 1] space.
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

/// RGB color value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Blend this color toward a background by alpha (0.0 = bg, 1.0 = this).
    pub fn blend(&self, bg: &Rgb, alpha: f32) -> Rgb {
        let a = alpha.clamp(0.0, 1.0);
        Rgb::new(
            (bg.r as f32 * (1.0 - a) + self.r as f32 * a) as u8,
            (bg.g as f32 * (1.0 - a) + self.g as f32 * a) as u8,
            (bg.b as f32 * (1.0 - a) + self.b as f32 * a) as u8,
        )
    }
}

/// High-level primitive for the animated substrate.
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
