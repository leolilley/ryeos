//! Typed identity for one source channel owned by one mounted view instance.
//!
//! The platform effect boundary still carries an opaque string. Encoding and
//! decoding live here so lifecycle code never parses tile, dock, or section
//! suffixes independently.

use crate::ids::RyeOsViewInstanceKey;

const PREFIX: &str = "rsk1";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RyeOsSourceChannel {
    Named(String),
    Mention(String),
    Completion(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RyeOsSourceInstanceKey {
    pub view_instance: RyeOsViewInstanceKey,
    pub channel: RyeOsSourceChannel,
}

impl RyeOsSourceInstanceKey {
    pub fn new(view_instance: RyeOsViewInstanceKey, channel: RyeOsSourceChannel) -> Self {
        Self {
            view_instance,
            channel,
        }
    }

    pub fn named(view_instance: RyeOsViewInstanceKey, channel: impl Into<String>) -> Self {
        Self::new(view_instance, RyeOsSourceChannel::Named(channel.into()))
    }

    pub fn mention(view_instance: RyeOsViewInstanceKey, input_id: impl Into<String>) -> Self {
        Self::new(view_instance, RyeOsSourceChannel::Mention(input_id.into()))
    }

    pub fn completion(view_instance: RyeOsViewInstanceKey, input_id: impl Into<String>) -> Self {
        Self::new(
            view_instance,
            RyeOsSourceChannel::Completion(input_id.into()),
        )
    }

    pub fn encode(&self) -> String {
        let host = encode_component(self.view_instance.as_str());
        match &self.channel {
            RyeOsSourceChannel::Named(channel) => {
                format!("{PREFIX}/{host}/source/{}", encode_component(channel))
            }
            RyeOsSourceChannel::Mention(input_id) => {
                format!("{PREFIX}/{host}/mention/{}", encode_component(input_id))
            }
            RyeOsSourceChannel::Completion(input_id) => {
                format!("{PREFIX}/{host}/completion/{}", encode_component(input_id))
            }
        }
    }

    pub fn decode(encoded: &str) -> Option<Self> {
        let parts = encoded.split('/').collect::<Vec<_>>();
        if parts.first().copied() != Some(PREFIX) {
            return None;
        }
        let host = decode_component(*parts.get(1)?)?;
        let view_instance = RyeOsViewInstanceKey::from_canonical(&host)?;
        let channel = match parts.as_slice() {
            [_, _, "source", channel] => RyeOsSourceChannel::Named(decode_component(channel)?),
            [_, _, "mention", input_id] => RyeOsSourceChannel::Mention(decode_component(input_id)?),
            [_, _, "completion", input_id] => {
                RyeOsSourceChannel::Completion(decode_component(input_id)?)
            }
            _ => return None,
        };
        let key = Self::new(view_instance, channel);
        (key.encode() == encoded).then_some(key)
    }

    pub fn belongs_to(&self, instance: &RyeOsViewInstanceKey) -> bool {
        &self.view_instance == instance
    }
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn decode_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = *bytes.get(index + 1)?;
        let low = *bytes.get(index + 2)?;
        decoded.push((hex(high)? << 4) | hex(low)?);
        index += 3;
    }
    String::from_utf8(decoded).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TileId;

    #[test]
    fn codec_round_trips_every_current_channel_without_collisions() {
        let tile = RyeOsViewInstanceKey::workspace_tile(TileId::new(7));
        let dock = RyeOsViewInstanceKey::surface_slot("left");
        let keys = [
            RyeOsSourceInstanceKey::named(tile.clone(), "default"),
            RyeOsSourceInstanceKey::named(tile.clone(), "project"),
            RyeOsSourceInstanceKey::named(tile, "execution/selected"),
            RyeOsSourceInstanceKey::mention(dock.clone(), "line/with space"),
            RyeOsSourceInstanceKey::completion(dock, "line/with space"),
        ];
        let encoded = keys
            .iter()
            .map(RyeOsSourceInstanceKey::encode)
            .collect::<Vec<_>>();
        assert_eq!(
            encoded
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            keys.len()
        );
        for (key, encoded) in keys.iter().zip(encoded) {
            assert_eq!(RyeOsSourceInstanceKey::decode(&encoded).as_ref(), Some(key));
        }
    }

    #[test]
    fn decoder_rejects_noncanonical_or_unknown_hosts() {
        for value in [
            "7#section0",
            "rsk1/tile%3A7/default/extra",
            "rsk1/tile%3A07/default",
            "rsk1/dock%3Amiddle/default",
            "rsk1/tile%3A7/mention/%2f",
        ] {
            assert!(RyeOsSourceInstanceKey::decode(value).is_none(), "{value}");
        }
    }
}
