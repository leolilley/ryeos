//! Length-prefixed messages over one byte stream.
//!
//! Each message is a 4-byte big-endian unsigned length followed by exactly
//! that many bytes. Every reader and writer names its own bound; a length of
//! zero or above the bound is refused before any message byte is read, so a
//! peer cannot make the reader allocate past its bound. End of stream is
//! accepted only on a message boundary.

use std::io::{ErrorKind, Read, Write};

use anyhow::{Context as _, Result, bail, ensure};

pub const LENGTH_PREFIX_BYTES: usize = 4;

fn checked_length(length: usize, maximum: usize) -> Result<u32> {
    ensure!(
        maximum > 0 && u32::try_from(maximum).is_ok(),
        "length-prefixed message bound is invalid"
    );
    ensure!(
        length > 0 && length <= maximum,
        "length-prefixed message is empty or exceeds its {maximum}-byte bound"
    );
    Ok(u32::try_from(length)?)
}

/// One framed message: prefix then bytes.
pub fn encode(message: &[u8], maximum: usize) -> Result<Vec<u8>> {
    let length = checked_length(message.len(), maximum)?;
    let mut framed = Vec::with_capacity(LENGTH_PREFIX_BYTES + message.len());
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(message);
    Ok(framed)
}

/// Split the first framed message off `bytes`, returning it and the rest.
/// Returns `None` when `bytes` does not yet hold the whole message.
pub fn decode(bytes: &[u8], maximum: usize) -> Result<Option<(&[u8], &[u8])>> {
    let Some((prefix, rest)) = bytes.split_first_chunk::<LENGTH_PREFIX_BYTES>() else {
        return Ok(None);
    };
    let length = usize::try_from(u32::from_be_bytes(*prefix))?;
    checked_length(length, maximum)?;
    if rest.len() < length {
        return Ok(None);
    }
    Ok(Some(rest.split_at(length)))
}

/// Write one framed message and flush it.
pub fn write_message(writer: &mut impl Write, message: &[u8], maximum: usize) -> Result<()> {
    writer
        .write_all(&encode(message, maximum)?)
        .context("write length-prefixed message")?;
    writer.flush().context("flush length-prefixed message")
}

/// Read one framed message. `Ok(None)` is end of stream on a message
/// boundary; end of stream inside a prefix or message is refused.
pub fn read_message(reader: &mut impl Read, maximum: usize) -> Result<Option<Vec<u8>>> {
    let mut prefix = [0_u8; LENGTH_PREFIX_BYTES];
    let mut filled = 0;
    while filled < LENGTH_PREFIX_BYTES {
        match reader.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => bail!("length-prefixed stream ended inside a length prefix"),
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("read length prefix"),
        }
    }
    let length = usize::try_from(u32::from_be_bytes(prefix))?;
    checked_length(length, maximum)?;
    let mut message = vec![0_u8; length];
    reader.read_exact(&mut message).map_err(|error| {
        if error.kind() == ErrorKind::UnexpectedEof {
            anyhow::anyhow!("length-prefixed stream ended inside a message")
        } else {
            anyhow::Error::new(error).context("read length-prefixed message")
        }
    })?;
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_is_a_big_endian_u32_length_then_the_exact_bytes() {
        assert_eq!(
            encode(b"abc", 16).unwrap(),
            [0, 0, 0, 3, b'a', b'b', b'c']
        );
        let long = vec![7_u8; 0x0102];
        assert_eq!(&encode(&long, 0x0102).unwrap()[..4], &[0, 0, 1, 2]);
    }

    #[test]
    fn empty_oversized_and_unrepresentable_messages_are_refused() {
        assert!(encode(b"", 16).is_err());
        assert!(encode(&[1; 17], 16).is_err());
        assert!(encode(b"a", 0).is_err());
        assert!(decode(&[0, 0, 0, 0], 16).is_err());
        assert!(decode(&[0, 0, 0, 17], 16).is_err());
        // A huge declared length is refused from the prefix alone.
        assert!(decode(&[0xff, 0xff, 0xff, 0xff], 16).is_err());
        let mut reader: &[u8] = &[0xff, 0xff, 0xff, 0xff];
        assert!(read_message(&mut reader, 16).is_err());
    }

    #[test]
    fn decode_splits_consecutive_messages_and_waits_for_partial_ones() {
        let mut stream = encode(b"first", 16).unwrap();
        stream.extend(encode(b"second", 16).unwrap());
        let (first, rest) = decode(&stream, 16).unwrap().unwrap();
        assert_eq!(first, b"first");
        let (second, rest) = decode(rest, 16).unwrap().unwrap();
        assert_eq!(second, b"second");
        assert!(rest.is_empty());
        assert!(decode(rest, 16).unwrap().is_none());
        assert!(decode(&stream[..2], 16).unwrap().is_none());
        assert!(decode(&stream[..6], 16).unwrap().is_none());
    }

    #[test]
    fn stream_round_trip_ends_cleanly_only_on_a_boundary() {
        let mut wire = Vec::new();
        write_message(&mut wire, b"one", 8).unwrap();
        write_message(&mut wire, b"two!", 8).unwrap();
        let mut reader = wire.as_slice();
        assert_eq!(read_message(&mut reader, 8).unwrap().unwrap(), b"one");
        assert_eq!(read_message(&mut reader, 8).unwrap().unwrap(), b"two!");
        assert!(read_message(&mut reader, 8).unwrap().is_none());

        let mut inside_prefix: &[u8] = &wire[..2];
        assert!(read_message(&mut inside_prefix, 8).is_err());
        let mut inside_message: &[u8] = &wire[..5];
        assert!(read_message(&mut inside_message, 8).is_err());
        // The reader's own bound applies, whatever the writer used.
        let mut tighter = wire.as_slice();
        assert!(read_message(&mut tighter, 2).is_err());
    }
}
