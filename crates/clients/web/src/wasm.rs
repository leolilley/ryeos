//! WASM bridge — core runs in the browser.
//!
//! The WASM binary IS the core crate. It holds AppModel in WASM memory,
//! runs update() and build_frame() in-process. JS just pumps events in
//! and reads rendered output out.

use wasm_bindgen::prelude::*;

use ryeos_client_base::frame::build_frame;
use ryeos_client_base::input::{InputEvent, Key};
use ryeos_client_base::layout::Rect;
use ryeos_client_base::model::AppModel;
use ryeos_client_base::store::Store;
use ryeos_client_base::update::{self, AppEvent};

use std::cell::RefCell;

// ---------------------------------------------------------------------------
// State — single-threaded WASM, safe to use thread_local RefCell
// ---------------------------------------------------------------------------

thread_local! {
    static STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

struct AppState {
    model: AppModel,
    pixel_buf: Vec<u8>,
}

// ---------------------------------------------------------------------------
// WASM exports — JS calls these
// ---------------------------------------------------------------------------

/// Initialize the app with viewport dimensions.
#[wasm_bindgen]
pub fn start(width: u16, height: u16) {
    let mut model = AppModel::new_default(".");
    model.runtime.viewport = Rect::new(0, 0, width, height);

    // Seed the 3D scene
    let store = Store::new();
    for _ in 0..60 {
        model.visual.animation.tick(16, &store);
    }

    STATE.with(|s| {
        *s.borrow_mut() = Some(AppState {
            model,
            pixel_buf: Vec::new(),
        });
    });

    render();
}

/// Advance animation by dt milliseconds.
#[wasm_bindgen]
pub fn tick(dt_ms: u32) {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let store = Store::new();
            state.model.visual.animation.tick(dt_ms as u64, &store);
            state.model.dirty = true;
        }
    });
    render();
}

/// Dispatch a keyboard event.
#[wasm_bindgen]
pub fn on_key(key_code: u32, shift: bool, ctrl: bool, alt: bool) {
    let key = map_key(key_code, shift, ctrl, alt);
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            update::update(&mut state.model, AppEvent::Input(InputEvent::Key(key)));
        }
    });
    render();
}

/// Resize the viewport.
#[wasm_bindgen]
pub fn on_resize(width: u16, height: u16) {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            update::update(&mut state.model, AppEvent::Resize { width, height });
        }
    });
    render();
}

// ---------------------------------------------------------------------------
// Internal: render current state → JS callbacks
// ---------------------------------------------------------------------------

fn render() {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        let Some(ref mut state) = *state else { return };

        let needed = ryeos_client_base::scene::RENDER_W * ryeos_client_base::scene::RENDER_H * 4;
        if state.pixel_buf.len() != needed {
            state.pixel_buf.resize(needed, 0);
        }

        // Build frame, rasterize directly into pixel buffer
        let frame = build_frame(&mut state.model);
        ryeos_client_base::scene::rasterize_to_rgba(
            &frame.background,
            &mut state.pixel_buf,
        );

        // Send raw RGBA bytes to JS — zero JSON, fixed resolution.
        setPixels(
            state.pixel_buf.as_ptr(),
            state.pixel_buf.len(),
            ryeos_client_base::scene::RENDER_W,
            ryeos_client_base::scene::RENDER_H,
        );
    });
}

// ---------------------------------------------------------------------------
// JS callbacks — JS implements these, WASM calls them
// ---------------------------------------------------------------------------

#[wasm_bindgen]
extern "C" {
    /// JS receives raw RGBA pixels and draws to Canvas.
    #[wasm_bindgen(js_namespace = window)]
    fn setPixels(ptr: *const u8, len: usize, width: usize, height: usize);
}

// ---------------------------------------------------------------------------
// Key mapping — JS keyCode → core Key enum
// ---------------------------------------------------------------------------

fn map_key(key_code: u32, _shift: bool, ctrl: bool, alt: bool) -> Key {
    if ctrl {
        // Ctrl+letter: key_code is the uppercase ASCII code
        if let Some(c) = char::from_u32(key_code) {
            let lc = c.to_ascii_lowercase();
            if lc >= 'a' && lc <= 'z' {
                return Key::Ctrl(lc);
            }
        }
    }

    if alt {
        if let Some(c) = char::from_u32(key_code) {
            return Key::Alt(c);
        }
    }

    match key_code {
        13 => Key::Enter,
        9 => Key::Tab,
        8 => Key::Backspace,
        46 => Key::Delete,
        27 => Key::Escape,
        37 => Key::ArrowLeft,
        38 => Key::ArrowUp,
        39 => Key::ArrowRight,
        40 => Key::ArrowDown,
        33 => Key::PageUp,
        34 => Key::PageDown,
        36 => Key::Home,
        35 => Key::End,
        32 => Key::Char(' '),
        kc => {
            if let Some(c) = char::from_u32(kc) {
                Key::Char(c)
            } else {
                Key::Escape
            }
        }
    }
}
