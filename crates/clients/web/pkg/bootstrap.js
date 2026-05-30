import { bootStudio } from "/ui/assets/studio_shell.js";

bootStudio(document.getElementById("app")).catch((error) => {
  console.error("RyeOS Studio boot failed", error);
  const app = document.getElementById("app");
  app.replaceChildren();
  const main = document.createElement("main");
  main.className = "boot-error";
  const title = document.createElement("h1");
  title.textContent = "RyeOS Studio boot failed";
  const detail = document.createElement("pre");
  detail.textContent = error?.message || String(error);
  main.append(title, detail);
  app.append(main);
});
