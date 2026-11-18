//! Images a tool returns.
//!
//! A tool result that is a JSON object whose only key is `result_parts` is a
//! list of text and image parts. Anything else is an ordinary result and takes
//! the ordinary path. The envelope is self-describing, so the tool item needs
//! no declaration, and it is untrusted either way: the bytes are checked here,
//! against the real header, the model's declared form and the signed limits,
//! before anything is stored.
//!
//! A malformed envelope is an error result the model can read. An image that
//! fails a check is dropped and the result says why, in a stable code, so the
//! run goes on. A model whose entry declares no image input is told so by an
//! error result (or a note, when the entry asks for that); nothing is stored.

use std::collections::BTreeSet;

use base64::Engine as _;
use serde_json::Value;

use ryeos_directive_definition::{ImageInputConfig, UnsupportedImagePolicy};
use ryeos_provider_contract::ImagePart;
use ryeos_runtime::callback::EventAttachment;

use crate::directive::ToolImagesConfig;
use crate::image_sniff::{SniffError, sniff};

pub const RESULT_PARTS_KEY: &str = "result_parts";

/// The most parts one envelope may hold.
const MAX_ENVELOPE_PARTS: usize = 64;

/// Text a result carries when the tool sent none, so a result with images
/// always begins with a text part.
const NO_TEXT_NOTE: &str = "[no text output]";

pub const ENVELOPE_INVALID: &str = "tool_result_parts_invalid";
pub const IMAGE_INPUT_UNSUPPORTED: &str = "image_input_unsupported";
pub const IMAGE_OVERSIZE: &str = "image_oversize";
pub const IMAGE_MEDIA_TYPE: &str = "image_media_type";
pub const IMAGE_DECODE: &str = "image_decode";
pub const IMAGE_LIMIT: &str = "image_limit";

/// What the run has already stored, restored from the record on resume: how
/// many images it has admitted, how many distinct bytes it holds, and which
/// attachments the node already stores, so a repeated image uploads nothing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImageRunState {
    pub images: u64,
    pub bytes: u64,
    pub stored: BTreeSet<String>,
}

impl ImageRunState {
    /// Counts one image the record already holds.
    pub fn record(&mut self, image: &ImagePart) {
        self.images += 1;
        if self.stored.insert(image.attachment.clone()) {
            self.bytes += image.byte_length;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum EnvelopePart {
    Text(String),
    Image { media_type: String, bytes: Vec<u8> },
}

#[derive(Debug, PartialEq)]
pub enum Envelope {
    /// An ordinary result.
    NotParts,
    /// A `result_parts` object that is not well formed.
    Invalid(String),
    Parts(Vec<EnvelopePart>),
}

/// Recognises a `result_parts` result.
pub fn parse_envelope(result: &Value) -> Envelope {
    let Some(object) = result.as_object() else {
        return Envelope::NotParts;
    };
    if object.len() != 1 {
        return Envelope::NotParts;
    }
    let Some(list) = object.get(RESULT_PARTS_KEY) else {
        return Envelope::NotParts;
    };
    let Some(list) = list.as_array() else {
        return Envelope::Invalid("result_parts must be an array".to_string());
    };
    if list.is_empty() || list.len() > MAX_ENVELOPE_PARTS {
        return Envelope::Invalid(format!(
            "result_parts must hold 1-{MAX_ENVELOPE_PARTS} parts"
        ));
    }
    let mut parts = Vec::with_capacity(list.len());
    for (index, part) in list.iter().enumerate() {
        match parse_part(part) {
            Ok(part) => parts.push(part),
            Err(message) => return Envelope::Invalid(format!("part {index}: {message}")),
        }
    }
    Envelope::Parts(parts)
}

fn parse_part(part: &Value) -> Result<EnvelopePart, String> {
    let object = part.as_object().ok_or("a part must be an object")?;
    let text_field = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing string field {name}"))
    };
    let only = |names: &[&str]| {
        if object.keys().all(|key| names.contains(&key.as_str())) {
            Ok(())
        } else {
            Err(format!("a part may only hold {names:?}"))
        }
    };
    match object.get("type").and_then(Value::as_str) {
        Some("text") => {
            only(&["type", "text"])?;
            Ok(EnvelopePart::Text(text_field("text")?.to_owned()))
        }
        Some("image") => {
            only(&["type", "media_type", "data_b64"])?;
            let media_type = text_field("media_type")?.to_owned();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(text_field("data_b64")?)
                .map_err(|_| "data_b64 is not valid base64".to_string())?;
            Ok(EnvelopePart::Image { media_type, bytes })
        }
        _ => Err("type must be \"text\" or \"image\"".to_string()),
    }
}

/// An image admitted for storage and the request.
#[derive(Debug)]
pub struct AdmittedImage {
    /// The recorded part: type, attachment hash and the size the header gave.
    pub part: ImagePart,
    pub bytes: Vec<u8>,
    /// The upload, absent when the run already stored these bytes.
    pub upload: Option<EventAttachment>,
}

/// What became of a result's parts.
#[derive(Debug)]
pub enum Verdict {
    /// The result is an error the model reads; nothing is stored.
    Refused {
        code: &'static str,
        message: String,
    },
    Settled(Settled),
}

#[derive(Debug, Default)]
pub struct Settled {
    /// The text parts joined, empty when the tool sent none.
    pub text: String,
    pub images: Vec<AdmittedImage>,
    /// The code and one-based position of each image that was dropped.
    pub dropped: Vec<(usize, &'static str)>,
}

impl Settled {
    /// The text the result carries before the result guard sees it: the tool's
    /// text, a note for each dropped image, or a note that there was none.
    pub fn model_text(&self) -> String {
        let mut text = if self.text.is_empty() && self.dropped.is_empty() {
            NO_TEXT_NOTE.to_string()
        } else if self.text.is_empty() {
            String::new()
        } else {
            self.text.clone()
        };
        for (position, code) in &self.dropped {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[image {position} not used: {code}]"));
        }
        text
    }
}

/// Checks a result's parts. `config` is the model's provider entry.
pub fn admit_parts(
    parts: Vec<EnvelopePart>,
    config: Option<&ImageInputConfig>,
    limits: &ToolImagesConfig,
    run: &mut ImageRunState,
) -> Verdict {
    let accepts = config.and_then(|config| config.accepts.as_ref());
    let has_images = parts
        .iter()
        .any(|part| matches!(part, EnvelopePart::Image { .. }));
    let policy = config.map_or(UnsupportedImagePolicy::default(), |config| {
        config.unsupported
    });
    if has_images && accepts.is_none() && policy == UnsupportedImagePolicy::Refuse {
        return Verdict::Refused {
            code: IMAGE_INPUT_UNSUPPORTED,
            message: "this model takes no image input; return text instead".to_string(),
        };
    }
    let per_image_limit = accepts.map_or(limits.max_bytes_per_image, |accepts| {
        limits.max_bytes_per_image.min(accepts.max_bytes_per_image)
    });
    let mut settled = Settled::default();
    let mut texts: Vec<String> = Vec::new();
    let mut position = 0usize;
    let mut result_images = 0u32;
    let mut result_bytes = 0u64;
    for part in parts {
        let (media_type, bytes) = match part {
            EnvelopePart::Text(text) => {
                texts.push(text);
                continue;
            }
            EnvelopePart::Image { media_type, bytes } => (media_type, bytes),
        };
        position += 1;
        let Some(accepts) = accepts else {
            settled.dropped.push((position, IMAGE_INPUT_UNSUPPORTED));
            continue;
        };
        let length = bytes.len() as u64;
        if bytes.is_empty() {
            settled.dropped.push((position, IMAGE_DECODE));
            continue;
        }
        if length > per_image_limit {
            settled.dropped.push((position, IMAGE_OVERSIZE));
            continue;
        }
        let facts = match sniff(&media_type, &bytes) {
            Ok(facts) => facts,
            Err(SniffError::MediaTypeMismatch) => {
                settled.dropped.push((position, IMAGE_MEDIA_TYPE));
                continue;
            }
            Err(SniffError::Undecodable) => {
                settled.dropped.push((position, IMAGE_DECODE));
                continue;
            }
        };
        if !accepts
            .media_types
            .iter()
            .any(|accepted| accepted == facts.media_type)
        {
            settled.dropped.push((position, IMAGE_MEDIA_TYPE));
            continue;
        }
        let Ok(upload) = EventAttachment::new(facts.media_type, bytes.clone()) else {
            settled.dropped.push((position, IMAGE_DECODE));
            continue;
        };
        let hash = upload.object_hash().to_owned();
        let new_bytes = !run.stored.contains(&hash);
        if result_images >= limits.max_images_per_result
            || result_bytes + length > limits.max_bytes_per_result
            || run.images >= u64::from(limits.max_images_per_run)
            || (new_bytes && run.bytes + length > limits.max_bytes_per_run)
        {
            settled.dropped.push((position, IMAGE_LIMIT));
            continue;
        }
        result_images += 1;
        result_bytes += length;
        run.images += 1;
        if new_bytes {
            run.bytes += length;
            run.stored.insert(hash.clone());
        }
        settled.images.push(AdmittedImage {
            part: ImagePart {
                media_type: facts.media_type.to_string(),
                attachment: hash,
                byte_length: length,
                width: Some(facts.width),
                height: Some(facts.height),
            },
            bytes,
            upload: new_bytes.then_some(upload),
        });
    }
    settled.text = texts.join("\n");
    Verdict::Settled(settled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn png(width: u32, height: u32, extra: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0, extra]);
        bytes
    }

    fn envelope(parts: Value) -> Value {
        json!({ "result_parts": parts })
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn config(unsupported: &str, accepts: bool) -> ImageInputConfig {
        let mut value = json!({"unsupported": unsupported});
        if accepts {
            value["accepts"] = json!({
                "media_types": ["image/png"],
                "max_bytes_per_image": 200,
                "max_images_per_request": 4,
                "text_part_template": {"type": "text", "text": "{text}"},
                "image_part_template": {"type": "image_url", "image_url": {"url": "{data_url}"}},
                "tool_result_note": "[{count}]",
                "deferred_label": "{index} {call_id}",
                "omitted_note": "[omitted]",
                "cost": {"pixels_per_token": 100, "max_tokens": 50, "unknown_dimensions_tokens": 50}
            });
        }
        serde_json::from_value(value).unwrap()
    }

    fn admit(
        parts: Vec<EnvelopePart>,
        config: &ImageInputConfig,
        limits: &ToolImagesConfig,
        run: &mut ImageRunState,
    ) -> Verdict {
        admit_parts(parts, Some(config), limits, run)
    }

    fn image(bytes: Vec<u8>) -> EnvelopePart {
        EnvelopePart::Image {
            media_type: "image/png".to_string(),
            bytes,
        }
    }

    #[test]
    fn only_a_lone_result_parts_object_is_an_envelope() {
        assert_eq!(parse_envelope(&json!({"ok": true})), Envelope::NotParts);
        assert_eq!(parse_envelope(&json!("text")), Envelope::NotParts);
        assert_eq!(
            parse_envelope(&json!({"result_parts": [], "other": 1})),
            Envelope::NotParts
        );
        let parsed = parse_envelope(&envelope(json!([
            {"type": "text", "text": "hi"},
            {"type": "image", "media_type": "image/png", "data_b64": b64(b"xyz")},
        ])));
        assert_eq!(
            parsed,
            Envelope::Parts(vec![
                EnvelopePart::Text("hi".to_string()),
                EnvelopePart::Image {
                    media_type: "image/png".to_string(),
                    bytes: b"xyz".to_vec()
                },
            ])
        );
    }

    #[test]
    fn a_malformed_envelope_is_invalid_with_the_reason() {
        for bad in [
            json!({"result_parts": "x"}),
            json!({"result_parts": []}),
            json!({"result_parts": [1]}),
            json!({"result_parts": [{"type": "audio"}]}),
            json!({"result_parts": [{"type": "text"}]}),
            json!({"result_parts": [{"type": "text", "text": "a", "extra": 1}]}),
            json!({"result_parts": [{"type": "image", "media_type": "image/png", "data_b64": "!!"}]}),
        ] {
            assert!(
                matches!(parse_envelope(&bad), Envelope::Invalid(_)),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_good_image_is_admitted_with_its_header_size_and_uploaded_once() {
        let config = config("refuse", true);
        let limits = ToolImagesConfig::default();
        let mut run = ImageRunState::default();
        let bytes = png(640, 480, 0);
        let Verdict::Settled(first) = admit(
            vec![EnvelopePart::Text("note".into()), image(bytes.clone())],
            &config,
            &limits,
            &mut run,
        ) else {
            panic!("refused");
        };
        assert_eq!(first.text, "note");
        assert_eq!(first.images.len(), 1);
        let admitted = &first.images[0];
        assert_eq!(
            (admitted.part.width, admitted.part.height),
            (Some(640), Some(480))
        );
        assert!(admitted.upload.is_some());
        assert_eq!(run.images, 1);
        assert_eq!(run.bytes, bytes.len() as u64);
        // The same bytes again: counted as an image, stored once.
        let Verdict::Settled(second) =
            admit(vec![image(bytes.clone())], &config, &limits, &mut run)
        else {
            panic!("refused");
        };
        assert!(second.images[0].upload.is_none());
        assert_eq!(second.images[0].part.attachment, admitted.part.attachment);
        assert_eq!(run.images, 2);
        assert_eq!(run.bytes, bytes.len() as u64);
    }

    #[test]
    fn a_model_without_image_input_is_refused_or_noted_by_its_policy() {
        let limits = ToolImagesConfig::default();
        let mut run = ImageRunState::default();
        let bytes = png(2, 2, 0);
        let refuse = config("refuse", false);
        let Verdict::Refused { code, .. } =
            admit(vec![image(bytes.clone())], &refuse, &limits, &mut run)
        else {
            panic!("not refused");
        };
        assert_eq!(code, IMAGE_INPUT_UNSUPPORTED);
        assert_eq!(run, ImageRunState::default());
        let note = config("text_note", false);
        let Verdict::Settled(settled) = admit(
            vec![EnvelopePart::Text("kept".into()), image(bytes)],
            &note,
            &limits,
            &mut run,
        ) else {
            panic!("refused");
        };
        assert!(settled.images.is_empty());
        assert_eq!(settled.dropped, vec![(1, IMAGE_INPUT_UNSUPPORTED)]);
        assert_eq!(
            settled.model_text(),
            "kept\n[image 1 not used: image_input_unsupported]"
        );
        assert_eq!(run, ImageRunState::default());
        // No `image_input` at all behaves as the default policy: refuse.
        let Verdict::Refused { code, .. } = admit_parts(
            vec![image(png(2, 2, 0))],
            None,
            &limits,
            &mut ImageRunState::default(),
        ) else {
            panic!("not refused");
        };
        assert_eq!(code, IMAGE_INPUT_UNSUPPORTED);
    }

    #[test]
    fn each_bad_image_is_dropped_with_its_stable_code_and_the_rest_go_on() {
        let config = config("refuse", true);
        let limits = ToolImagesConfig::default();
        let mut run = ImageRunState::default();
        let oversize = {
            let mut bytes = png(2, 2, 0);
            bytes.extend(vec![0u8; 300]);
            bytes
        };
        let verdict = admit(
            vec![
                image(oversize),
                image(b"GIF89a\x01\x00\x01\x00".to_vec()),
                image(b"not an image at all".to_vec()),
                image(png(2, 2, 1)),
            ],
            &config,
            &limits,
            &mut run,
        );
        let Verdict::Settled(settled) = verdict else {
            panic!("refused");
        };
        assert_eq!(
            settled.dropped,
            vec![
                (1, IMAGE_OVERSIZE),
                (2, IMAGE_MEDIA_TYPE),
                (3, IMAGE_DECODE)
            ]
        );
        assert_eq!(settled.images.len(), 1);
        assert_eq!(run.images, 1);
    }

    #[test]
    fn the_per_result_and_per_run_caps_refuse_past_the_limit() {
        let config = config("refuse", true);
        let limits = ToolImagesConfig {
            max_images_per_result: 2,
            max_images_per_run: 3,
            ..ToolImagesConfig::default()
        };
        let mut run = ImageRunState::default();
        let result = |run: &mut ImageRunState, seeds: &[u8]| {
            let parts = seeds.iter().map(|s| image(png(2, 2, *s))).collect();
            let Verdict::Settled(settled) = admit(parts, &config, &limits, run) else {
                panic!("refused");
            };
            settled
        };
        let first = result(&mut run, &[1, 2, 3]);
        assert_eq!(first.images.len(), 2);
        assert_eq!(first.dropped, vec![(3, IMAGE_LIMIT)]);
        let second = result(&mut run, &[4, 5]);
        assert_eq!(second.images.len(), 1);
        assert_eq!(second.dropped, vec![(2, IMAGE_LIMIT)]);
        assert_eq!(run.images, 3);
    }

    #[test]
    fn a_result_of_only_images_begins_with_a_generated_text_part() {
        let settled = Settled {
            text: String::new(),
            images: vec![AdmittedImage {
                part: ImagePart {
                    media_type: "image/png".to_string(),
                    attachment: "0".repeat(64),
                    byte_length: 1,
                    width: Some(1),
                    height: Some(1),
                },
                bytes: vec![1],
                upload: None,
            }],
            dropped: Vec::new(),
        };
        assert_eq!(settled.model_text(), NO_TEXT_NOTE);
    }
}
