//! Web UI server — serves the TUI in a browser using the same core Frame
//! as the terminal. No duplicated logic: core builds the Frame, render_dom
//! and render_canvas render it.

use ryeos_tui_core::frame::{build_frame, Frame};
use ryeos_tui_core::input::{InputEvent, Key};
use ryeos_tui_core::layout::Rect;
use ryeos_tui_core::model::AppModel;
use ryeos_tui_core::store::Store;
use ryeos_tui_core::update::{self, AppEvent};

use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Shared app state — the model behind a mutex.
type SharedModel = Arc<Mutex<AppModel>>;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        run_server().await;
    });
}

async fn run_server() {
    let mut model = AppModel::new_default(".");
    model.runtime.viewport = Rect::new(0, 0, 200, 56);

    // Tick the animation to initialize the 3D scene
    let store = Store::new();
    for _ in 0..60 {
        model.visual.animation.tick(16, &store);
    }

    let state: SharedModel = Arc::new(Mutex::new(model));

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/api/frame", get(get_frame))
        .route("/api/tick", post(post_tick))
        .route("/api/input", post(post_input))
        .with_state(state);

    let addr = "127.0.0.1:4200";
    eprintln!("ryeos-tui-web listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

/// GET / — serves the HTML shell
async fn serve_index() -> Html<String> {
    Html(index_html())
}

/// GET /api/frame — returns the current frame as JSON
async fn get_frame(State(state): State<SharedModel>) -> Json<Value> {
    let model = state.lock().unwrap();
    let frame = build_frame(&model);
    Json(serde_json::to_value(render_frame_json(&frame)).unwrap())
}

/// POST /api/tick — advance animation by one frame (16ms)
async fn post_tick(State(state): State<SharedModel>) -> Json<Value> {
    let mut model = state.lock().unwrap();
    let store = Store::new();
    model.visual.animation.tick(16, &store);
    model.dirty = true;
    let frame = build_frame(&model);
    Json(serde_json::to_value(render_frame_json(&frame)).unwrap())
}

/// POST /api/input — send a keyboard event and get the updated frame
async fn post_input(
    State(state): State<SharedModel>,
    Json(body): Json<InputBody>,
) -> Json<Value> {
    let mut model = state.lock().unwrap();

    let event = match body.event.as_str() {
        "key" => {
            let key = parse_key(&body.key, body.shift, body.ctrl, body.alt);
            AppEvent::Input(InputEvent::Key(key))
        }
        "resize" => {
            let w = body.width.unwrap_or(200);
            let h = body.height.unwrap_or(56);
            AppEvent::Resize { width: w, height: h }
        }
        _ => {
            // Tick by default
            let store = Store::new();
            model.visual.animation.tick(16, &store);
            model.dirty = true;
            let frame = build_frame(&model);
            return Json(serde_json::to_value(render_frame_json(&frame)).unwrap());
        }
    };

    let _effects = update::update(&mut model, event);
    let frame = build_frame(&model);
    Json(serde_json::to_value(render_frame_json(&frame)).unwrap())
}

#[derive(Debug, Deserialize)]
struct InputBody {
    event: String,
    key: Option<String>,
    shift: Option<bool>,
    ctrl: Option<bool>,
    alt: Option<bool>,
    width: Option<u16>,
    height: Option<u16>,
}

// ---------------------------------------------------------------------------
// Frame → JSON (the single source of truth both renderers consume)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct FrameJson {
    tiles: Vec<TileJson>,
    status_bar: String,
    input_bar: String,
    input_hint: String,
    primitives: Vec<Value>,
}

#[derive(Serialize)]
struct TileJson {
    id: String,
    title: String,
    focused: bool,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    html: String,
}

fn render_frame_json(frame: &Frame) -> FrameJson {
    // Render tile surfaces via render_dom
    let tiles: Vec<TileJson> = frame
        .tiles
        .iter()
        .map(|t| {
            let html = ryeos_tui_web::render_dom::generate_html(&t.cells);
            TileJson {
                id: format!("{:?}", t.tile_id),
                title: html.clone(), // V1: use the rendered cells directly
                focused: false,      // V1: not tracked on TileSurface
                x: t.rect.x,
                y: t.rect.y,
                w: t.rect.w,
                h: t.rect.h,
                html,
            }
        })
        .collect();

    let status_bar = ryeos_tui_web::render_dom::generate_html(&frame.status_bar.cells);
    let input_bar = ryeos_tui_web::render_dom::generate_html(&frame.input.cells);

    // Serialize primitives for the Canvas renderer
    let primitives: Vec<Value> = frame
        .background
        .iter()
        .map(|p| serde_json::to_value(p).unwrap())
        .collect();

    FrameJson {
        tiles,
        status_bar,
        input_bar,
        input_hint: String::new(), // V1: hint is rendered inside the input bar cells
        primitives,
    }
}

// ---------------------------------------------------------------------------
// Key parsing
// ---------------------------------------------------------------------------

fn parse_key(key: &Option<String>, shift: Option<bool>, ctrl: Option<bool>, alt: Option<bool>) -> Key {
    let k = key.as_deref().unwrap_or("");
    let ctrl = ctrl.unwrap_or(false);
    let _shift = shift.unwrap_or(false);
    let _alt = alt.unwrap_or(false);

    if ctrl {
        match k {
            "s" => return Key::Ctrl('s'),
            "v" => return Key::Ctrl('v'),
            "x" => return Key::Ctrl('x'),
            "r" => return Key::Ctrl('r'),
            "c" => return Key::Ctrl('c'),
            _ => {}
        }
    }

    match k {
        "Enter" => Key::Enter,
        "Tab" => Key::Tab,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Escape" => Key::Escape,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Home" => Key::Home,
        "End" => Key::End,
        " " => Key::Char(' '),
        ":" => Key::Char(':'),
        "/" => Key::Char('/'),
        "?" => Key::Char('?'),
        s if s.len() == 1 => Key::Char(s.chars().next().unwrap()),
        _ => Key::Escape,
    }
}

// ---------------------------------------------------------------------------
// HTML shell — the browser hosts the grid + canvas
// ---------------------------------------------------------------------------

fn index_html() -> String {
    r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Rye OS</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    background: #1d2021;
    color: #ebdbb2;
    font-family: 'JetBrains Mono', 'Fira Code', monospace;
    overflow: hidden;
    height: 100vh;
  }
  #container {
    position: relative;
    width: 100vw;
    height: 100vh;
  }
  #bg-canvas {
    position: absolute;
    top: 0; left: 0;
    width: 100%; height: 100%;
    z-index: 0;
  }
  #grid {
    position: absolute;
    top: 0; left: 0;
    z-index: 10;
    white-space: pre;
    font-size: 14px;
    line-height: 18px;
    letter-spacing: 0px;
    padding: 0;
  }
  #grid .tile {
    position: absolute;
    border: 1px solid #3c3836;
    background: rgba(29,32,33,0.88);
    overflow: hidden;
  }
  #grid .tile.focused {
    border-color: #fe8019;
  }
  #grid .status-bar {
    position: absolute;
    left: 0;
    background: rgba(29,32,33,0.95);
  }
  #grid .input-bar {
    position: absolute;
    left: 0;
    background: rgba(29,32,33,0.95);
  }
</style>
</head>
<body>
<div id="container">
  <canvas id="bg-canvas"></canvas>
  <div id="grid"></div>
</div>
<script>
const canvas = document.getElementById('bg-canvas');
const ctx = canvas.getContext('2d');
const grid = document.getElementById('grid');

let W, H;
function resize() {
  W = canvas.width = window.innerWidth;
  H = canvas.height = window.innerHeight;
}
window.addEventListener('resize', resize);
resize();

// Measure cell dimensions from the grid font
let cellW = 8.4, cellH = 18;
function measureCell() {
  const probe = document.createElement('span');
  probe.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;font-size:14px;line-height:18px;font-family:JetBrains Mono,Fira Code,monospace';
  probe.textContent = 'X'.repeat(100);
  grid.appendChild(probe);
  cellW = probe.getBoundingClientRect().width / 100;
  cellH = 18;
  probe.remove();
}

// Render Canvas from primitives (same data the terminal Braille renderer uses)
function renderPrimitives(prims) {
  ctx.clearRect(0, 0, W, H);
  ctx.fillStyle = '#1d2021';
  ctx.fillRect(0, 0, W, H);
  for (const p of prims) {
    switch (p.type) {
      case 'Point': {
        ctx.fillStyle = rgb(p.color);
        ctx.globalAlpha = p.opacity;
        ctx.beginPath();
        ctx.arc(p.pos.x * W, p.pos.y * H, Math.max(0.5, p.size * 50), 0, Math.PI * 2);
        ctx.fill();
        break;
      }
      case 'Line': {
        ctx.strokeStyle = rgb(p.color);
        ctx.globalAlpha = p.opacity;
        ctx.lineWidth = Math.max(0.5, p.thickness * 2);
        ctx.beginPath();
        ctx.moveTo(p.from.x * W, p.from.y * H);
        ctx.lineTo(p.to.x * W, p.to.y * H);
        ctx.stroke();
        break;
      }
      case 'Ring': {
        ctx.strokeStyle = rgb(p.color);
        ctx.globalAlpha = p.opacity;
        ctx.lineWidth = 1.5;
        const rx = p.radius * W * 0.5;
        const ry = rx * Math.cos(p.tilt);
        ctx.beginPath();
        ctx.ellipse(p.center.x * W, p.center.y * H, rx, ry, p.rotation, 0, Math.PI * 2);
        ctx.stroke();
        break;
      }
      case 'Polygon': {
        if (p.vertices.length < 2) break;
        ctx.strokeStyle = rgb(p.color);
        ctx.globalAlpha = p.opacity;
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        ctx.moveTo(p.vertices[0].x * W, p.vertices[0].y * H);
        for (let i = 1; i < p.vertices.length; i++) {
          ctx.lineTo(p.vertices[i].x * W, p.vertices[i].y * H);
        }
        ctx.closePath();
        ctx.stroke();
        break;
      }
    }
  }
  ctx.globalAlpha = 1;
}

function rgb(c) { return '#' + [c.r, c.g, c.b].map(v => v.toString(16).padStart(2,'0')).join(''); }

// Render tile grid from the frame JSON
function renderGrid(data) {
  let html = '';
  for (const tile of data.tiles) {
    const style = `left:${tile.x * cellW}px;top:${tile.y * cellH}px;width:${tile.w * cellW}px;height:${tile.h * cellH}px;`;
    const cls = 'tile' + (tile.focused ? ' focused' : '');
    html += `<div class="${cls}" style="${style}" data-tile="${tile.id}"><b style="color:#fe8019;padding:0 4px">${esc(tile.title)}</b>${tile.html}</div>`;
  }
  // Status bar
  const sb = data.status_bar;
  // Input bar
  const ib = data.input_bar;
  grid.innerHTML = html;
  renderPrimitives(data.primitives);
}

function esc(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }

// Fetch initial frame then poll for animation
async function fetchFrame(body) {
  const res = await fetch('/api/input', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body || { event: 'tick' }),
  });
  return res.json();
}

async function init() {
  measureCell();
  const data = await fetch('/api/frame').then(r => r.json());
  renderGrid(data);

  // Animation loop: tick every 50ms
  setInterval(async () => {
    const data = await fetchFrame({ event: 'tick' });
    renderGrid(data);
  }, 50);

  // Keyboard input
  document.addEventListener('keydown', async (e) => {
    if (['ArrowUp','ArrowDown','ArrowLeft','ArrowRight','Tab',' '].includes(e.key)) {
      e.preventDefault();
    }
    const body = {
      event: 'key',
      key: e.key,
      shift: e.shiftKey,
      ctrl: e.ctrlKey,
      alt: e.altKey,
    };
    const data = await fetchFrame(body);
    renderGrid(data);
  });
}

init();
</script>
</body>
</html>"##.to_string()
}
