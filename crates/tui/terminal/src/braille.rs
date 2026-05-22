//! Braille subcell framebuffer for scene primitive rasterization.
//!
//! Each terminal cell has 8 subcell dots arranged as:
//!
//!   (0) (1)
//!   (2) (3)
//!   (4) (5)
//!   (6) (7)
//!
//! Braille codepoint = 0x2800 + dot_bits

/// Set a subcell dot in the braille cell.
/// (col, row) are in [0,1] × [0,3] subcell coordinates.
/// Uses standard Braille dot-to-bit mapping:
///   Dot 1 = (0,0) → bit 0    Dot 4 = (1,0) → bit 3
///   Dot 2 = (0,1) → bit 1    Dot 5 = (1,1) → bit 4
///   Dot 3 = (0,2) → bit 2    Dot 6 = (1,2) → bit 5
///   Dot 7 = (0,3) → bit 6    Dot 8 = (1,3) → bit 7
pub fn set_braille_dot(bits: u8, col: usize, row: usize) -> u8 {
    let dot_index = match (col, row) {
        (0, 0) => 0, // dot 1
        (0, 1) => 1, // dot 2
        (0, 2) => 2, // dot 3
        (1, 0) => 3, // dot 4
        (1, 1) => 4, // dot 5
        (1, 2) => 5, // dot 6
        (0, 3) => 6, // dot 7
        (1, 3) => 7, // dot 8
        _ => return bits,
    };
    bits | (1 << dot_index)
}

/// Convert braille dot bits to a Unicode braille character.
pub fn braille_char(bits: u8) -> char {
    char::from_u32(0x2800 + bits as u32).unwrap_or(' ')
}

/// Subcell framebuffer for a region of the terminal.
pub struct BrailleBuffer {
    pub width: usize,  // in terminal cells
    pub height: usize, // in terminal cells
    bits: Vec<u8>,     // one byte per cell (8 dots)
}

impl BrailleBuffer {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            bits: vec![0; width * height],
        }
    }

    pub fn clear(&mut self) {
        for b in self.bits.iter_mut() {
            *b = 0;
        }
    }

    /// Plot a subcell point. (x, y) in subcell coords (2x per cell, 4y per cell).
    pub fn plot(&mut self, sub_x: usize, sub_y: usize) {
        let cell_x = sub_x / 2;
        let cell_y = sub_y / 4;
        if cell_x < self.width && cell_y < self.height {
            let col = sub_x % 2;
            let row = sub_y % 4;
            let idx = cell_y * self.width + cell_x;
            self.bits[idx] = set_braille_dot(self.bits[idx], col, row);
        }
    }

    /// Plot a line from (x0, y0) to (x1, y1) in subcell coords using Bresenham.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut cx = x0;
        let mut cy = y0;

        loop {
            if cx >= 0 && cy >= 0 {
                self.plot(cx as usize, cy as usize);
            }
            if cx == x1 && cy == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                cx += sx;
            }
            if e2 <= dx {
                err += dx;
                cy += sy;
            }
        }
    }

    /// Get the braille character for a cell.
    pub fn get_char(&self, cell_x: usize, cell_y: usize) -> char {
        if cell_x < self.width && cell_y < self.height {
            braille_char(self.bits[cell_y * self.width + cell_x])
        } else {
            ' '
        }
    }

    /// Get the raw bits for a cell.
    pub fn get_bits(&self, cell_x: usize, cell_y: usize) -> u8 {
        if cell_x < self.width && cell_y < self.height {
            self.bits[cell_y * self.width + cell_x]
        } else {
            0
        }
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
    fn braille_buffer_plot_and_line() {
        let mut buf = BrailleBuffer::new(10, 5);
        buf.plot(0, 0);
        assert_eq!(buf.get_char(0, 0), '⠁');

        buf.line(0, 0, 18, 8);
        // Should have drawn something along the line
        assert_ne!(buf.get_char(0, 0), '⠀');
    }
}
