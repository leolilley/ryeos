//! Images in a provider request.
//!
//! The record keeps an image where it happened: inside the tool result that
//! returned it, as a hash. A Chat Completions request carries images only in
//! user messages, so at build time the images of each run of tool messages move
//! into one user message that follows the run. This reads only the record and
//! the provider entry, so a live run, a crash resume and a continuation build
//! the same bytes.
//!
//! A request carries only the newest `max_images_per_request` images; older
//! ones stand in as a short text note, in place. The estimate applies the same
//! rule, so what is counted is what is sent.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use ryeos_directive_definition::{ImageAcceptConfig, ImageInputConfig, render_note};
use ryeos_provider_contract::{ImagePart, Message, Part, Role};

use crate::directive::ProviderMessage;

/// Largest total of image bytes held in memory at once. The oldest are
/// dropped first and read back from the daemon when a request needs them.
const IMAGE_STORE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// The bytes of an image a request may carry, kept as base64 because that is
/// the form every request renders.
#[derive(Debug, Clone)]
pub struct StoredImage {
    pub media_type: String,
    pub base64: String,
}

/// Images by the hash of their attachment object, bounded in memory.
#[derive(Debug)]
pub struct ImageStore {
    entries: HashMap<String, StoredImage>,
    order: VecDeque<String>,
    bytes: usize,
    /// Images eviction must leave: the ones the next request carries.
    pinned: HashSet<String>,
    limit: usize,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            pinned: HashSet::new(),
            limit: IMAGE_STORE_MAX_BYTES,
        }
    }
}

impl ImageStore {
    pub fn insert(&mut self, attachment: &str, media_type: &str, bytes: &[u8]) {
        if self.entries.contains_key(attachment) {
            return;
        }
        let base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.bytes += base64.len();
        self.entries.insert(
            attachment.to_owned(),
            StoredImage {
                media_type: media_type.to_owned(),
                base64,
            },
        );
        self.order.push_back(attachment.to_owned());
        // Evict the oldest image that is neither pinned nor the one just
        // inserted; when only those remain the store stays over its bound
        // rather than lose an image a request needs.
        while self.bytes > self.limit {
            let Some(at) = self
                .order
                .iter()
                .position(|held| held != attachment && !self.pinned.contains(held))
            else {
                break;
            };
            if let Some(oldest) = self.order.remove(at)
                && let Some(dropped) = self.entries.remove(&oldest)
            {
                self.bytes -= dropped.base64.len();
            }
        }
    }

    /// Protects exactly these images from eviction, replacing any earlier pin.
    pub fn pin(&mut self, attachments: impl IntoIterator<Item = String>) {
        self.pinned = attachments.into_iter().collect();
    }

    pub fn contains(&self, attachment: &str) -> bool {
        self.entries.contains_key(attachment)
    }

    pub fn get(&self, attachment: &str) -> Option<&StoredImage> {
        self.entries.get(attachment)
    }
}

/// Replaces, in every message, each image the model's entry cannot take by
/// size with a text note that names `code`: an image whose bytes exceed the
/// entry's per-image limit, or whose base64 would not fit one callback frame
/// when read back. Whether an image fails depends only on its recorded size
/// and the entry, so a live run, a resume and a continuation all replace the
/// same images. Returns how many were replaced.
pub fn replace_unsendable_images(
    messages: &mut [ProviderMessage],
    accepts: &ImageAcceptConfig,
    code: &str,
) -> usize {
    let fails = |image: &ImagePart| {
        image.byte_length > accepts.max_bytes_per_image
            || image.byte_length.div_ceil(3).saturating_mul(4) + READ_FRAME_HEADROOM_BYTES
                > READ_FRAME_BYTES
    };
    let note = |_: &ImagePart| Part::text(format!("[image not used: {code}]"));
    let mut replaced = 0;
    for message in messages.iter_mut() {
        for part in message.parts.iter_mut() {
            match part {
                Part::Image(image) if fails(image) => {
                    *part = note(image);
                    replaced += 1;
                }
                Part::ToolResult(result) => {
                    for inner in result.parts.iter_mut() {
                        if let Part::Image(image) = inner
                            && fails(image)
                        {
                            *inner = note(image);
                            replaced += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    replaced
}

/// The most a callback frame carries: a stored image is read back whole in
/// one frame, as base64 inside a small JSON wrapper.
const READ_FRAME_BYTES: u64 = 10 * 1024 * 1024;
const READ_FRAME_HEADROOM_BYTES: u64 = 4096;

/// The images moved out of a run of tool messages: per call, whether each is
/// carried by the request, and the image.
type Deferred = Vec<(String, Vec<(bool, ImagePart)>)>;

/// Every image of a history, oldest first: message order, then part order,
/// then order inside a tool result. A request carries the images from some
/// position on, so the position of an image is its ordinal in this order and
/// does not depend on which messages a request leaves out. `lower_images`
/// counts the same way: a top-level image only in a message that is not a tool
/// message, a result's images only inside a tool message. A validated message
/// can hold an image only in a user message (`Part::allowed_in`), so a tool
/// message never has a top-level one.
fn image_parts(messages: &[ProviderMessage]) -> Vec<&ImagePart> {
    let mut images = Vec::new();
    for message in messages {
        for part in &message.parts {
            match part {
                Part::Image(image) => {
                    debug_assert!(
                        message.role != Role::Tool,
                        "a tool message holds no top-level image"
                    );
                    images.push(image)
                }
                Part::ToolResult(result) => {
                    images.extend(result.parts.iter().filter_map(|inner| match inner {
                        Part::Image(image) => Some(image),
                        _ => None,
                    }))
                }
                _ => {}
            }
        }
    }
    images
}

/// The images of a history a request carries: the newest `max`.
pub fn retained_images(messages: &[ProviderMessage], max: u32) -> Vec<&ImagePart> {
    let images = image_parts(messages);
    let skip = images.len().saturating_sub(max as usize);
    images.into_iter().skip(skip).collect()
}

/// What a request is built with when its history holds images: the entry's
/// form, the bytes, and which images are carried.
#[derive(Debug)]
pub struct ImageRender<'a> {
    accepts: &'a ImageAcceptConfig,
    store: &'a ImageStore,
    /// The ordinal of the oldest image the request carries.
    first_retained: usize,
}

impl<'a> ImageRender<'a> {
    /// `Ok(None)` when the history holds no image. Otherwise the entry must
    /// declare that the model takes images, every carried image must be of a
    /// type it takes, and its bytes must be in `store`: a history that holds
    /// an image the request cannot send is refused, never sent without it.
    pub fn prepare(
        config: Option<&'a ImageInputConfig>,
        store: &'a ImageStore,
        messages: &[ProviderMessage],
    ) -> Result<Option<Self>> {
        let images = image_parts(messages);
        if images.is_empty() {
            return Ok(None);
        }
        let Some(accepts) = config.and_then(|config| config.accepts.as_ref()) else {
            bail!(
                "image_input_unsupported: the conversation holds {} image(s) and this model's provider entry declares no image input; zero provider requests",
                images.len()
            );
        };
        let first_retained = images
            .len()
            .saturating_sub(accepts.max_images_per_request as usize);
        for image in &images[first_retained..] {
            if !accepts.media_types.contains(&image.media_type) {
                bail!(
                    "image_input_unsupported: the conversation holds a {} image and this model takes {:?}; zero provider requests",
                    image.media_type,
                    accepts.media_types
                );
            }
            if !store.contains(&image.attachment) {
                bail!(
                    "image_attachment_missing: the bytes of image {} are not loaded; zero provider requests",
                    image.attachment
                );
            }
        }
        Ok(Some(Self {
            accepts,
            store,
            first_retained,
        }))
    }

    fn omitted_note(&self, image: &ImagePart) -> String {
        let dimension =
            |value: Option<u32>| value.map_or_else(|| "?".to_string(), |v| v.to_string());
        render_note(
            &self.accepts.omitted_note,
            &[
                ("media_type", image.media_type.clone()),
                ("width", dimension(image.width)),
                ("height", dimension(image.height)),
                ("byte_length", image.byte_length.to_string()),
            ],
        )
    }

    fn image_part(&self, image: &ImagePart) -> Option<Value> {
        use ryeos_runtime::template::apply_template;
        let stored = self.store.get(&image.attachment)?;
        let mut context: HashMap<String, Value> = HashMap::new();
        context.insert(
            "data_url".to_string(),
            json!(format!(
                "data:{};base64,{}",
                stored.media_type, stored.base64
            )),
        );
        context.insert("data_b64".to_string(), json!(stored.base64));
        context.insert("media_type".to_string(), json!(stored.media_type));
        context.insert("width".to_string(), json!(image.width));
        context.insert("height".to_string(), json!(image.height));
        Some(apply_template(&self.accepts.image_part_template, &context))
    }

    fn text_part(&self, text: &str) -> Value {
        use ryeos_runtime::template::apply_template;
        let mut context: HashMap<String, Value> = HashMap::new();
        context.insert("text".to_string(), json!(text));
        apply_template(&self.accepts.text_part_template, &context)
    }

    /// The content array of a user message: its text and image parts in
    /// order, each rendered from the entry's templates.
    pub fn user_content(&self, message: &ProviderMessage) -> Value {
        let mut blocks = Vec::new();
        for part in &message.parts {
            match part {
                Part::Text(text) => blocks.push(self.text_part(&text.text)),
                Part::Image(image) => match self.image_part(image) {
                    Some(block) => blocks.push(block),
                    // The bytes were checked present when the render was
                    // prepared, so this is unreachable; the note keeps the
                    // model from being told nothing if it ever is not.
                    None => blocks.push(self.text_part(&self.omitted_note(image))),
                },
                _ => {}
            }
        }
        Value::Array(blocks)
    }
}

/// Whether the message is a user message that holds an image part.
pub fn user_holds_image(message: &ProviderMessage) -> bool {
    message.role == Role::User
        && message
            .parts
            .iter()
            .any(|part| matches!(part, Part::Image(_)))
}

/// Moves the images of each run of tool messages into one user message after
/// the run, leaving a note in the tool result, and replaces images the request
/// does not carry with their note. Each entry pairs the message with the index
/// of the message it came from in `messages`; the added user message carries
/// the index of the last tool message of its run.
pub fn lower_images(
    messages: &[ProviderMessage],
    render: Option<&ImageRender<'_>>,
) -> Vec<(usize, ProviderMessage)> {
    let Some(render) = render else {
        return messages.iter().cloned().enumerate().collect();
    };
    let mut out: Vec<(usize, ProviderMessage)> = Vec::with_capacity(messages.len());
    // Images moved out of the current run of tool messages, per call.
    let mut deferred: Deferred = Vec::new();
    let mut last_tool = 0usize;
    // The ordinal of the next image in history order.
    let mut ordinal = 0usize;
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::Tool {
            flush_run(&mut out, &mut deferred, last_tool, render);
            out.push((index, omit_unsent_images(message, render, &mut ordinal)));
            continue;
        }
        last_tool = index;
        let mut parts = Vec::with_capacity(message.parts.len());
        for part in &message.parts {
            let Part::ToolResult(result) = part else {
                debug_assert!(
                    !matches!(part, Part::Image(_)),
                    "a tool message holds no top-level image, so none goes uncounted"
                );
                parts.push(part.clone());
                continue;
            };
            let mut kept = Vec::with_capacity(result.parts.len() + 1);
            let mut moved: Vec<(bool, ImagePart)> = Vec::new();
            for inner in &result.parts {
                match inner {
                    Part::Image(image) => {
                        moved.push((ordinal >= render.first_retained, image.clone()));
                        ordinal += 1;
                    }
                    other => kept.push(other.clone()),
                }
            }
            if !moved.is_empty() {
                kept.push(Part::text(format!(
                    "\n{}",
                    render_note(
                        &render.accepts.tool_result_note,
                        &[("count", moved.len().to_string())]
                    )
                )));
                deferred.push((result.call_id.clone(), moved));
            }
            let mut lowered = result.clone();
            lowered.parts = kept;
            parts.push(Part::ToolResult(lowered));
        }
        out.push((index, Message::new(Role::Tool, parts)));
    }
    flush_run(&mut out, &mut deferred, last_tool, render);
    out
}

fn flush_run(
    out: &mut Vec<(usize, ProviderMessage)>,
    deferred: &mut Deferred,
    last_tool: usize,
    render: &ImageRender<'_>,
) {
    if deferred.is_empty() {
        return;
    }
    let mut parts = Vec::new();
    for (call_id, images) in deferred.drain(..) {
        for (position, (carried, image)) in images.into_iter().enumerate() {
            if carried {
                parts.push(Part::text(render_note(
                    &render.accepts.deferred_label,
                    &[
                        ("index", (position + 1).to_string()),
                        ("call_id", call_id.clone()),
                    ],
                )));
                parts.push(Part::Image(image));
            } else {
                parts.push(Part::text(render.omitted_note(&image)));
            }
        }
    }
    out.push((last_tool, Message::new(Role::User, parts)));
}

/// A message other than a tool message, with any image the request does not
/// carry replaced by its note.
fn omit_unsent_images(
    message: &ProviderMessage,
    render: &ImageRender<'_>,
    ordinal: &mut usize,
) -> ProviderMessage {
    if !message
        .parts
        .iter()
        .any(|part| matches!(part, Part::Image(_)))
    {
        return message.clone();
    }
    let parts = message
        .parts
        .iter()
        .map(|part| match part {
            Part::Image(image) => {
                let carried = *ordinal >= render.first_retained;
                *ordinal += 1;
                if carried {
                    part.clone()
                } else {
                    Part::text(render.omitted_note(image))
                }
            }
            other => other.clone(),
        })
        .collect();
    Message::new(message.role, parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::ProviderMessageExt;
    use ryeos_provider_contract::ToolResultStatus;

    fn accepts(max: u32) -> ImageInputConfig {
        serde_json::from_value(json!({
            "accepts": {
                "media_types": ["image/png"],
                "max_bytes_per_image": 1000,
                "max_images_per_request": max,
                "text_part_template": {"type": "text", "text": "{text}"},
                "image_part_template": {"type": "image_url", "image_url": {"url": "{data_url}"}},
                "tool_result_note": "[{count} image(s) follow]",
                "deferred_label": "Image {index} from {call_id}:",
                "omitted_note": "[omitted {media_type} {width}x{height}]",
                "cost": {"pixels_per_token": 750, "max_tokens": 1600, "unknown_dimensions_tokens": 1600}
            }
        }))
        .unwrap()
    }

    fn hash(seed: char) -> String {
        std::iter::repeat_n(seed, 64).collect()
    }

    fn image(seed: char) -> ImagePart {
        ImagePart {
            media_type: "image/png".to_string(),
            attachment: hash(seed),
            byte_length: 6,
            width: Some(2),
            height: Some(3),
        }
    }

    fn tool_with_images(call_id: &str, text: &str, seeds: &[char]) -> ProviderMessage {
        let mut parts = vec![Part::text(text)];
        parts.extend(seeds.iter().map(|seed| Part::Image(image(*seed))));
        Message::new(
            Role::Tool,
            vec![Part::tool_result(call_id, parts, ToolResultStatus::Ok)],
        )
    }

    #[test]
    fn eviction_leaves_pinned_images_and_the_one_just_inserted() {
        let mut store = ImageStore {
            limit: 20,
            ..ImageStore::default()
        };
        // Each 6-byte image is 8 bytes of base64: two fit, a third evicts.
        for seed in ['a', 'b'] {
            store.insert(&hash(seed), "image/png", b"123456");
        }
        store.pin([hash('a')]);
        store.insert(&hash('c'), "image/png", b"123456");
        assert!(store.contains(&hash('a')), "pinned image evicted");
        assert!(!store.contains(&hash('b')), "oldest unpinned stays");
        assert!(store.contains(&hash('c')));
        // With everything else pinned the store goes over its bound rather
        // than lose an image the request needs.
        store.pin([hash('a'), hash('c'), hash('d')]);
        store.insert(&hash('d'), "image/png", b"123456");
        assert!(store.contains(&hash('a')) && store.contains(&hash('c')));
        assert!(store.contains(&hash('d')));
    }

    #[test]
    fn an_image_too_large_to_send_becomes_a_note_everywhere_it_appears() {
        let accepts = accepts(4);
        let accepts = accepts.accepts.as_ref().unwrap();
        let mut big = image('b');
        big.byte_length = 1001;
        let mut messages = vec![
            Message::new(
                Role::User,
                vec![Part::Image(big.clone()), Part::Image(image('a'))],
            ),
            Message::new(
                Role::Tool,
                vec![Part::tool_result(
                    "c1",
                    vec![Part::text("t"), Part::Image(big)],
                    ToolResultStatus::Ok,
                )],
            ),
        ];
        let replaced = replace_unsendable_images(&mut messages, accepts, "image_oversize");
        assert_eq!(replaced, 2);
        assert_eq!(
            retained_images(&messages, 10)
                .iter()
                .map(|image| image.attachment.clone())
                .collect::<Vec<_>>(),
            vec![hash('a')]
        );
        assert!(
            matches!(&messages[0].parts[0], Part::Text(text) if text.text == "[image not used: image_oversize]")
        );
        // Nothing left to replace, so a second pass is a no-op.
        assert_eq!(
            replace_unsendable_images(&mut messages, accepts, "image_oversize"),
            0
        );
    }

    fn store_with(seeds: &[char]) -> ImageStore {
        let mut store = ImageStore::default();
        for seed in seeds {
            store.insert(
                &hash(*seed),
                "image/png",
                format!("bytes-{seed}").as_bytes(),
            );
        }
        store
    }

    fn url(seed: char) -> String {
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(format!("bytes-{seed}"))
        )
    }

    #[test]
    fn a_history_without_images_needs_no_render_and_lowers_to_itself() {
        let messages = vec![
            ProviderMessage::user("hi"),
            ProviderMessage::tool_result("c", "ok"),
        ];
        let store = ImageStore::default();
        let config = accepts(2);
        assert!(
            ImageRender::prepare(Some(&config), &store, &messages)
                .unwrap()
                .is_none()
        );
        let lowered = lower_images(&messages, None);
        assert_eq!(lowered.len(), 2);
        assert_eq!(lowered[1], (1, messages[1].clone()));
    }

    #[test]
    fn one_image_moves_into_a_user_message_after_the_tool_message() {
        let messages = vec![
            ProviderMessage::user("go"),
            tool_with_images("c1", "saw it", &['a']),
            ProviderMessage::assistant_text("done"),
        ];
        let store = store_with(&['a']);
        let config = accepts(5);
        let render = ImageRender::prepare(Some(&config), &store, &messages)
            .unwrap()
            .unwrap();
        let lowered = lower_images(&messages, Some(&render));
        assert_eq!(lowered.len(), 4);
        assert_eq!(lowered[1].1.text().unwrap(), "saw it\n[1 image(s) follow]");
        assert!(retained_images(std::slice::from_ref(&lowered[1].1), 10).is_empty());
        let user = &lowered[2].1;
        assert_eq!(user.role, Role::User);
        assert_eq!(lowered[2].0, 1);
        assert_eq!(
            render.user_content(user),
            json!([
                {"type": "text", "text": "Image 1 from c1:"},
                {"type": "image_url", "image_url": {"url": url('a')}}
            ])
        );
        assert_eq!(lowered[3].0, 2);
    }

    #[test]
    fn the_images_of_two_calls_in_one_run_share_one_user_message_in_call_order() {
        let messages = vec![
            tool_with_images("c1", "first", &['a', 'b']),
            tool_with_images("c2", "second", &['c']),
            ProviderMessage::assistant_text("done"),
        ];
        let store = store_with(&['a', 'b', 'c']);
        let config = accepts(5);
        let render = ImageRender::prepare(Some(&config), &store, &messages)
            .unwrap()
            .unwrap();
        let lowered = lower_images(&messages, Some(&render));
        assert_eq!(lowered.len(), 4);
        let content = render.user_content(&lowered[2].1);
        let labels: Vec<&str> = content
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect();
        assert_eq!(
            labels,
            ["Image 1 from c1:", "Image 2 from c1:", "Image 1 from c2:"]
        );
        assert_eq!(content[1]["image_url"]["url"], url('a'));
        assert_eq!(content[5]["image_url"]["url"], url('c'));
    }

    #[test]
    fn only_the_newest_images_are_carried_and_older_ones_become_notes_in_place() {
        let messages = vec![
            tool_with_images("c1", "one", &['a', 'b']),
            ProviderMessage::assistant_text("mid"),
            tool_with_images("c2", "two", &['c', 'd']),
        ];
        // Only the newest two bytes need be present.
        let store = store_with(&['c', 'd']);
        let config = accepts(2);
        let render = ImageRender::prepare(Some(&config), &store, &messages)
            .unwrap()
            .unwrap();
        let lowered = lower_images(&messages, Some(&render));
        let first = render.user_content(&lowered[1].1);
        assert_eq!(
            first,
            json!([
                {"type": "text", "text": "[omitted image/png 2x3]"},
                {"type": "text", "text": "[omitted image/png 2x3]"}
            ])
        );
        let second = render.user_content(&lowered.last().unwrap().1);
        assert_eq!(second[1]["image_url"]["url"], url('c'));
        assert_eq!(second[3]["image_url"]["url"], url('d'));
        // Same input, same output.
        let again = lower_images(&messages, Some(&render));
        assert_eq!(lowered, again);
        assert_eq!(retained_images(&messages, 2).len(), 2);
    }

    #[test]
    fn an_image_the_model_cannot_take_refuses_the_request() {
        let messages = vec![tool_with_images("c1", "one", &['a'])];
        let store = store_with(&['a']);
        let error = |config: Option<&ImageInputConfig>, store: &ImageStore| {
            ImageRender::prepare(config, store, &messages)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default()
        };
        assert!(error(None, &store).contains("image_input_unsupported"));
        let blind = ImageInputConfig::default();
        assert!(error(Some(&blind), &store).contains("image_input_unsupported"));
        let mut jpeg_only = accepts(2);
        jpeg_only.accepts.as_mut().unwrap().media_types = vec!["image/jpeg".to_string()];
        assert!(error(Some(&jpeg_only), &store).contains("image_input_unsupported"));
        let config = accepts(2);
        assert!(error(Some(&config), &ImageStore::default()).contains("image_attachment_missing"));
    }

    #[test]
    fn the_store_drops_the_oldest_past_its_bound_and_keeps_the_newest() {
        let mut store = ImageStore::default();
        let big = vec![0u8; IMAGE_STORE_MAX_BYTES / 2];
        store.insert(&hash('a'), "image/png", &big);
        store.insert(&hash('b'), "image/png", &big);
        assert!(!store.contains(&hash('a')));
        assert!(store.contains(&hash('b')));
    }
}
