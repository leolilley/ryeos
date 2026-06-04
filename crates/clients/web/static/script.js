// Rye OS — Web JS bridge
//
// Thin adapter: loads WASM, pumps events in, reads pixel output.
// Core rasterizes 3D scene to RGBA in WASM, JS just blits to Canvas.

import init, { start, tick, on_key, on_resize } from './ryeos_tui_web.js';

const canvas = document.getElementById('bg-canvas');
const ctx = canvas.getContext('2d');

let W, H;

function resize() {
  W = canvas.width = window.innerWidth;
  H = canvas.height = window.innerHeight;
}

// --- Canvas rendering (raw RGBA from WASM memory) ---

let wasmExports;
let imgData = null;

window.setPixels = function (ptr, len, width, height) {
  if (width === 0 || height === 0) return;

  const bytes = new Uint8Array(wasmExports.memory.buffer, ptr, len);

  // Create ImageData matching the WASM dimensions, then scale to canvas
  if (!imgData || imgData.width !== width || imgData.height !== height) {
    imgData = ctx.createImageData(width, height);
  }

  // Copy RGBA bytes directly into ImageData
  imgData.data.set(bytes);

  // Use an offscreen canvas at the WASM resolution, then scale up
  const offscreen = new OffscreenCanvas(width, height);
  const offCtx = offscreen.getContext('2d');
  offCtx.putImageData(imgData, 0, 0);

  // Scale to fill viewport
  ctx.imageSmoothingEnabled = true;
  ctx.clearRect(0, 0, W, H);
  ctx.drawImage(offscreen, 0, 0, W, H);
};

// --- Boot ---

async function boot() {
  wasmExports = await init();

  resize();

  const cols = Math.floor(W / 8.4);
  const rows = Math.floor(H / 18);
  start(cols, rows);

  // Animation loop: tick WASM every 66ms (~15fps — smooth for the scene)
  setInterval(() => tick(66), 66);

  // Keyboard
  document.addEventListener('keydown', (e) => {
    const blocked = ['ArrowUp','ArrowDown','ArrowLeft','ArrowRight','Tab',' '];
    if (blocked.includes(e.key)) e.preventDefault();

    on_key(
      e.key.length === 1 ? e.key.charCodeAt(0) : keyCode(e),
      e.shiftKey, e.ctrlKey, e.altKey
    );
  });

  // Resize
  let resizeTimer;
  window.addEventListener('resize', () => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      resize();
      const cols = Math.floor(W / 8.4);
      const rows = Math.floor(H / 18);
      on_resize(cols, rows);
    }, 100);
  });
}

function keyCode(e) {
  switch (e.key) {
    case 'Enter': return 13;
    case 'Tab': return 9;
    case 'Backspace': return 8;
    case 'Delete': return 46;
    case 'Escape': return 27;
    case 'ArrowLeft': return 37;
    case 'ArrowUp': return 38;
    case 'ArrowRight': return 39;
    case 'ArrowDown': return 40;
    case 'PageUp': return 33;
    case 'PageDown': return 34;
    case 'Home': return 36;
    case 'End': return 35;
    default: return 0;
  }
}

boot();
