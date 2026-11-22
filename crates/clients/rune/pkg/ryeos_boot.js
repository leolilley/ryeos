// Loads the Rune renderer. The renderer reads the session the launch cookie names and
// draws everything else, including its own boot and failure screens.
//
// On the web Rune draws only the faces it is given, so the coverage fonts (CJK and
// emoji) load beside the renderer and are handed over before it starts. A font that
// fails to load leaves its characters undrawn; the client still starts.
import init, { add_font, start } from "./ryeos_rune.js";

const FONTS = ["ryeos-cjk.ttf", "ryeos-emoji.ttf"];

async function font(name) {
  try {
    const response = await fetch(new URL(name, import.meta.url));
    if (!response.ok) throw new Error(`${response.status}`);
    return new Uint8Array(await response.arrayBuffer());
  } catch (error) {
    console.warn(`RyeOS font ${name} did not load: ${error}`);
    return null;
  }
}

// The page draws the renderer's starting frame until the renderer has drawn its own.
const boot = document.getElementById("ryeos-boot");
try {
  const [, ...fonts] = await Promise.all([init(), ...FONTS.map(font)]);
  for (const bytes of fonts) {
    if (bytes) add_font(bytes);
  }
  start();
  requestAnimationFrame(() => requestAnimationFrame(() => boot?.remove()));
} catch (error) {
  const head = boot?.querySelector(".ryeos-head");
  if (head) head.textContent = "RyeOS could not start";
  const status = document.getElementById("ryeos-status");
  if (status) status.textContent = `${error}`;
  console.error(error);
}
