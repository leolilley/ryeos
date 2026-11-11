// Loads the Rune renderer. The renderer reads the session the launch cookie names and
// draws everything else, including its own boot and failure screens.
import init from "./ryeos_rune.js";

const status = document.getElementById("ryeos-status");
try {
  await init();
  status.textContent = "";
} catch (error) {
  status.textContent = `RyeOS could not start: ${error}`;
  console.error(error);
}
