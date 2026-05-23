//! Braille subcell framebuffer for scene primitive rasterization.
//!
//! Each terminal cell has 8 subcell dots arranged as:
//!
//!   (col=0,row=0) (col=1,row=0)
//!   (col=0,row=1) (col=1,row=1)
//!   (col=0,row=2) (col=1,row=2)
//!   (col=0,row=3) (col=1,row=3)
//!
//! Braille codepoint = 0x2800 + dot_bits
//! Effective resolution: cols×2 wide, rows×4 tall.

use ryeos_tui_core::scene::Rgb;

// Standard Braille dot-to-bit mapping:
//   Dot 1 = (0,0) → bit 0    Dot 4 = (1,0) → bit 3
//   Dot 2 = (0,1) → bit 1    Dot 5 = (1,1) → bit 4
//   Dot 3 = (0,2) → bit 2    Dot 6 = (1,2) → bit 5
//   Dot 7 = (0,3) → bit 6    Dot 8 = (1,3) → bit 7
const DOT_BIT: [[u8; 4]; 2] = [
    [0, 1, 2, 6], // left column
    [3, 4, 5, 7], // right column
];

/// Set a subcell dot in the braille cell.
pub fn set_braille_dot(bits: u8, col: usize, row: usize) -> u8 {
    if col < 2 && row < 4 {
        bits | (1 << DOT_BIT[col][row])
    } else {
        bits
    }
}

/// Convert braille dot bits to a Unicode braille character.
pub fn braille_char(bits: u8) -> char {
    char::from_u32(0x2800 + bits as u32).unwrap_or(' ')
}

/// Colored subcell framebuffer — tracks dots + per-cell color.
/// Braille pixel space: width*2 × height*4.
pub struct ColoredBrailleBuffer {
    pub width: usize,  // terminal cells
    pub height: usize, // terminal cells
    bits: Vec<u8>,
    colors: Vec<Rgb>,
    // Accumulate brightness per cell for Wu line intensity
    brightness: Vec<f32>,
}

impl ColoredBrailleBuffer {
    pub fn new(width: usize, height: usize) -> Self {
        let bg = Rgb::new(0x1d, 0x20, 0x21);
        Self {
            width,
            height,
            bits: vec![0; width * height],
            colors: vec![bg; width * height],
            brightness: vec![0.0; width * height],
        }
    }

    pub fn clear(&mut self) {
        let bg = Rgb::new(0x1d, 0x20, 0x21);
        for b in self.bits.iter_mut() { *b = 0; }
        for c in self.colors.iter_mut() { *c = bg; }
        for b in self.brightness.iter_mut() { *b = 0.0; }
    }

    /// Braille pixel dimensions.
    pub fn pixel_w(&self) -> usize { self.width * 2 }
    pub fn pixel_h(&self) -> usize { self.height * 4 }

    /// Set a dot in braille pixel space with sub-cell positioning.
    /// (px, py) in [0, pixel_w) × [0, pixel_h).
    pub fn plot(&mut self, px: usize, py: usize, color: Rgb, intensity: f32) {
        let cell_x = px / 2;
        let cell_y = py / 4;
        if cell_x < self.width && cell_y < self.height {
            let dot_col = px % 2;
            let dot_row = py % 4;
            let idx = cell_y * self.width + cell_x;
            self.bits[idx] = set_braille_dot(self.bits[idx], dot_col, dot_row);
            // Blend color: brighter wins
            if intensity > self.brightness[idx] {
                self.brightness[idx] = intensity;
                self.colors[idx] = color;
            }
        }
    }

    /// Wu's antialiased line in braille pixel space.
    /// Uses intensity threshold — only sets dot if bright enough.
    pub fn wu_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, color: Rgb, opacity: f32) {
        let steep = (y1 - y0).abs() > (x1 - x0).abs();

        let (ax, ay, bx, by) = if steep {
            (y0, x0, y1, x1)
        } else {
            (x0, y0, x1, y1)
        };

        // Ensure left-to-right
        let (ax, ay, bx, by) = if ax > bx {
            (bx, by, ax, ay)
        } else {
            (ax, ay, bx, by)
        };

        let dx = bx - ax;
        let dy = by - ay;
        let gradient = if dx.abs() < 0.001 { 1.0 } else { dy / dx };

        // First endpoint
        let xend = ax.round();
        let yend = ay + gradient * (xend - ax);
        let xgap = 1.0 - ((ax + 0.5).fract());
        let xpxl1 = xend as isize;
        let fpart = yend - yend.floor();

        if steep {
            self.plot_checked(xpxl1, yend.floor() as isize, color, opacity * (1.0 - fpart) * xgap);
            self.plot_checked(xpxl1, yend.floor() as isize + 1, color, opacity * fpart * xgap);
        } else {
            self.plot_checked(xpxl1, yend.floor() as isize, color, opacity * (1.0 - fpart) * xgap);
            self.plot_checked(xpxl1, yend.floor() as isize + 1, color, opacity * fpart * xgap);
        }

        let mut intery = yend + gradient;

        // Second endpoint
        let xend = bx.round();
        let yend = by + gradient * (xend - bx);
        let xgap = (bx + 0.5).fract();
        let xpxl2 = xend as isize;
        let fpart = yend - yend.floor();

        if steep {
            self.plot_checked(xpxl2, yend.floor() as isize, color, opacity * (1.0 - fpart) * xgap);
            self.plot_checked(xpxl2, yend.floor() as isize + 1, color, opacity * fpart * xgap);
        } else {
            self.plot_checked(xpxl2, yend.floor() as isize, color, opacity * (1.0 - fpart) * xgap);
            self.plot_checked(xpxl2, yend.floor() as isize + 1, color, opacity * fpart * xgap);
        }

        // Main loop
        if steep {
            for x in (xpxl1 + 1)..xpxl2 {
                let fy = intery.floor() as isize;
                let fpart = intery - intery.floor();
                self.plot_checked(fy, x, color, opacity * (1.0 - fpart));
                self.plot_checked(fy + 1, x, color, opacity * fpart);
                intery += gradient;
            }
        } else {
            for x in (xpxl1 + 1)..xpxl2 {
                let fy = intery.floor() as isize;
                let fpart = intery - intery.floor();
                self.plot_checked(x, fy, color, opacity * (1.0 - fpart));
                self.plot_checked(x, fy + 1, color, opacity * fpart);
                intery += gradient;
            }
        }
    }

    /// Plot a point in braille pixel space, checked bounds.
    fn plot_checked(&mut self, px: isize, py: isize, color: Rgb, intensity: f32) {
        if px < 0 || py < 0 || intensity < 0.1 {
            return;
        }
        // Scale color by intensity so dim dots are darker
        let scaled = Rgb::new(
            (color.r as f32 * intensity) as u8,
            (color.g as f32 * intensity) as u8,
            (color.b as f32 * intensity) as u8,
        );
        self.plot(px as usize, py as usize, scaled, intensity);
    }

    /// Get the braille character for a cell.
    pub fn get_char(&self, cell_x: usize, cell_y: usize) -> char {
        if cell_x < self.width && cell_y < self.height {
            braille_char(self.bits[cell_y * self.width + cell_x])
        } else {
            ' '
        }
    }

    /// Get the color for a cell.
    pub fn get_color(&self, cell_x: usize, cell_y: usize) -> Rgb {
        if cell_x < self.width && cell_y < self.height {
            self.colors[cell_y * self.width + cell_x]
        } else {
            Rgb::new(0x1d, 0x20, 0x21)
        }
    }

    /// Convert to ANSI string with truecolor fg on dark bg.
    pub fn to_ansi(&self) -> String {
        let mut out = String::with_capacity(self.width * self.height * 20);
        for row in 0..self.height {
            for col in 0..self.width {
                let idx = row * self.width + col;
                let bits = self.bits[idx];
                if bits == 0 {
                    // Empty cell — just space with bg color
                    out.push(' ');
                } else {
                    let color = self.colors[idx];
                    // Set fg color, output braille char
                    use std::fmt::Write;
                    write!(out, "\x1b[38;2;{}\x1b[48;2;29;32;33m",
                        format!("{};{};{}m", color.r, color.g, color.b)
                    ).unwrap();
                    out.push(braille_char(bits));
                }
            }
            out.push('\n');
        }
        out.push_str("\x1b[0m");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braille_dot_mapping_is_stable() {
        let mut bits = 0u8;
        bits = set_braille_dot(bits, 0, 0); // dot 1, bit 0
        assert_eq!(bits, 0b0000_0001);
        bits = set_braille_dot(bits, 1, 0); // dot 4, bit 3
        assert_eq!(bits, 0b0000_1001);
        bits = set_braille_dot(bits, 0, 3); // dot 7, bit 6
        assert_eq!(bits, 0b0100_1001);
    }

    #[test]
    fn braille_char_produces_valid_braille() {
        assert_eq!(braille_char(0), '⠀');
        assert_eq!(braille_char(0xFF), '⣿');
    }

    #[test]
    fn wu_line_sets_dots() {
        let mut buf = ColoredBrailleBuffer::new(40, 12);
        let pw = buf.pixel_w() as f32;
        let ph = buf.pixel_h() as f32;
        // Draw a horizontal line across the middle
        buf.wu_line(0.0, ph / 2.0, pw - 1.0, ph / 2.0, Rgb::new(0xff, 0x80, 0x00), 1.0);
        // Should have set dots in the middle row
        assert!(buf.bits.iter().any(|&b| b != 0), "wu line should set dots");
    }

    #[test]
    fn to_ansi_produces_output() {
        let mut buf = ColoredBrailleBuffer::new(10, 5);
        buf.plot(3, 7, Rgb::new(0xff, 0x80, 0x00), 1.0);
        let ansi = buf.to_ansi();
        assert!(ansi.contains("254;128;0") || ansi.contains("255;128;0") || ansi.contains("⠠") || ansi.contains("⡀"));
    }
}
