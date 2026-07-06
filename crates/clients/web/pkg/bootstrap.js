import { bootRyeOs } from "/ui/assets/ryeos_shell.js";

bootRyeOs(document.getElementById("app")).catch((error) => {
  console.error("RyeOS boot failed", error);
  const app = document.getElementById("app");
  app.replaceChildren();
  const main = document.createElement("main");
  main.className = "boot-error";
  const title = document.createElement("h1");
  title.textContent = "RyeOS boot failed";
  const detail = document.createElement("pre");
  detail.textContent = error?.message || String(error);
  main.append(title, detail);
  app.append(main);
});
