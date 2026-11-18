//! Reads an image's real format and size from its header bytes.
//!
//! A tool's claim about what it returned is not evidence: the declared media
//! type is checked against the magic bytes, and the width and height come from
//! the header, so no imaging dependency is needed and a tool cannot misstate
//! a size. Four formats are read. A valid image of another format is refused
//! as undecodable; support for one more format is one more arm here.

/// What the header of a supported image says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageFacts {
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SniffError {
    /// The bytes are a supported image of a different type than declared.
    MediaTypeMismatch,
    /// The bytes are not a supported image, or the header is cut short.
    Undecodable,
}

/// The facts of `bytes`, which must be an image of the `declared` media type.
pub fn sniff(declared: &str, bytes: &[u8]) -> Result<ImageFacts, SniffError> {
    let facts = read_header(bytes).ok_or(SniffError::Undecodable)?;
    if facts.media_type != declared {
        return Err(SniffError::MediaTypeMismatch);
    }
    Ok(facts)
}

fn read_header(bytes: &[u8]) -> Option<ImageFacts> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png(bytes);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return gif(bytes);
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        return jpeg(bytes);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return webp(bytes);
    }
    None
}

fn facts(media_type: &'static str, width: u32, height: u32) -> Option<ImageFacts> {
    (width > 0 && height > 0).then_some(ImageFacts {
        media_type,
        width,
        height,
    })
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(raw))
}

fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    let raw: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
    Some(u32::from(u16::from_be_bytes(raw)))
}

fn le16(bytes: &[u8], at: usize) -> Option<u32> {
    let raw: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
    Some(u32::from(u16::from_le_bytes(raw)))
}

fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let raw = bytes.get(at..at + 3)?;
    Some(u32::from(raw[0]) | (u32::from(raw[1]) << 8) | (u32::from(raw[2]) << 16))
}

fn png(bytes: &[u8]) -> Option<ImageFacts> {
    // The first chunk must be IHDR: length, "IHDR", width, height.
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    facts("image/png", be32(bytes, 16)?, be32(bytes, 20)?)
}

fn gif(bytes: &[u8]) -> Option<ImageFacts> {
    facts("image/gif", le16(bytes, 6)?, le16(bytes, 8)?)
}

fn jpeg(bytes: &[u8]) -> Option<ImageFacts> {
    let mut at = 2;
    loop {
        // Skip to the next marker: 0xff, then a non-0xff, non-zero byte.
        while *bytes.get(at)? != 0xff {
            at += 1;
        }
        while *bytes.get(at)? == 0xff {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        match marker {
            // Standalone markers carry no length.
            0x00 | 0x01 | 0xd0..=0xd8 => continue,
            // End of image or start of scan before any frame header.
            0xd9 | 0xda => return None,
            _ => {}
        }
        let length = usize::try_from(be16(bytes, at)?).ok()?;
        if length < 2 {
            return None;
        }
        let is_frame_header =
            matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_frame_header {
            // Segment: length(2), precision(1), height(2), width(2).
            return facts("image/jpeg", be16(bytes, at + 5)?, be16(bytes, at + 3)?);
        }
        at += length;
    }
}

fn webp(bytes: &[u8]) -> Option<ImageFacts> {
    match bytes.get(12..16)? {
        b"VP8 " => {
            // Frame tag (3 bytes), start code 9d 01 2a, then 14-bit sizes.
            if bytes.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            facts(
                "image/webp",
                le16(bytes, 26)? & 0x3fff,
                le16(bytes, 28)? & 0x3fff,
            )
        }
        b"VP8L" => {
            if *bytes.get(20)? != 0x2f {
                return None;
            }
            let packed = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
            facts(
                "image/webp",
                (packed & 0x3fff) + 1,
                ((packed >> 14) & 0x3fff) + 1,
            )
        }
        b"VP8X" => facts(
            "image/webp",
            le24(bytes, 24)?.checked_add(1)?,
            le24(bytes, 27)?.checked_add(1)?,
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
        bytes
    }

    fn gif_bytes(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes
    }

    fn jpeg_bytes(width: u16, height: u16) -> Vec<u8> {
        // SOI, an APP0 segment to skip, then SOF0.
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x4a, 0x46];
        bytes.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes
    }

    fn webp_lossy(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"RIFF\x00\x00\x00\x00WEBPVP8 \x00\x00\x00\x00".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0x9d, 0x01, 0x2a]);
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes
    }

    fn webp_lossless(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"RIFF\x00\x00\x00\x00WEBPVP8L\x00\x00\x00\x00\x2f".to_vec();
        let packed = (width - 1) | ((height - 1) << 14);
        bytes.extend_from_slice(&packed.to_le_bytes());
        bytes
    }

    fn webp_extended(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"RIFF\x00\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00\x00\x00\x00\x00".to_vec();
        bytes.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        bytes.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        bytes
    }

    #[test]
    fn each_format_reports_its_own_size() {
        let seen = |media: &str, bytes: Vec<u8>| {
            let facts = sniff(media, &bytes).unwrap();
            (facts.width, facts.height)
        };
        assert_eq!(seen("image/png", png_bytes(640, 480)), (640, 480));
        assert_eq!(seen("image/gif", gif_bytes(31, 17)), (31, 17));
        assert_eq!(seen("image/jpeg", jpeg_bytes(1024, 768)), (1024, 768));
        assert_eq!(seen("image/webp", webp_lossy(200, 100)), (200, 100));
        assert_eq!(seen("image/webp", webp_lossless(321, 123)), (321, 123));
        assert_eq!(seen("image/webp", webp_extended(4000, 3000)), (4000, 3000));
    }

    #[test]
    fn a_declared_type_that_differs_from_the_bytes_is_a_mismatch() {
        assert_eq!(
            sniff("image/jpeg", &png_bytes(2, 2)),
            Err(SniffError::MediaTypeMismatch)
        );
    }

    #[test]
    fn unknown_bytes_and_cut_headers_are_undecodable() {
        assert_eq!(
            sniff("image/png", b"not an image"),
            Err(SniffError::Undecodable)
        );
        assert_eq!(
            sniff("image/png", &png_bytes(2, 2)[..20]),
            Err(SniffError::Undecodable)
        );
        assert_eq!(
            sniff("image/jpeg", &[0xff, 0xd8, 0xff]),
            Err(SniffError::Undecodable)
        );
        assert_eq!(
            sniff("image/gif", b"GIF89a\x01"),
            Err(SniffError::Undecodable)
        );
        assert_eq!(
            sniff("image/png", &png_bytes(0, 4)),
            Err(SniffError::Undecodable)
        );
        assert_eq!(sniff("image/png", b""), Err(SniffError::Undecodable));
    }
}
