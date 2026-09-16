const app = document.getElementById("app");
const stage = document.getElementById("ryeos-boot-stage");
const detail = document.getElementById("ryeos-boot-detail");

function setBootStage(nextStage, nextDetail) {
  if (stage) stage.textContent = nextStage;
  if (detail) detail.textContent = nextDetail;
}

async function start() {
  setBootStage("Loading signed interface", "Resolving the installed RyeOS surface.");
  const { bootRyeOs } = await import("/ui/assets/ryeos_shell.js");
  setBootStage("Joining node", "Opening the authenticated browser session and durable seat.");
  await bootRyeOs(app);
}

start().catch((error) => {
  console.error("RyeOS boot failed", error);
  app.replaceChildren();
  const main = document.createElement("main");
  main.className = "ryeos-boot ryeos-boot-failed";
  main.setAttribute("role", "alert");
  const kicker = document.createElement("p");
  kicker.className = "ryeos-boot-kicker";
  kicker.textContent = "RyeOS / connection interrupted";
  const title = document.createElement("h1");
  title.textContent = "The node surface did not open";
  const detail = document.createElement("pre");
  detail.className = "ryeos-boot-error-detail";
  detail.textContent = error?.message || String(error);
  const guidance = document.createElement("p");
  guidance.className = "ryeos-boot-detail";
  guidance.textContent = "Run ryeos web again to mint a fresh one-time launch, or check ryeos node status if the node is offline.";
  const retry = document.createElement("button");
  retry.className = "ryeos-boot-retry";
  retry.type = "button";
  retry.textContent = "Retry this session";
  retry.addEventListener("click", () => location.reload());
  main.append(kicker, title, detail, guidance, retry);
  app.append(main);
});
