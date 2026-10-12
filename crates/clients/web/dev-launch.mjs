// Host-side development entrypoint, not worker execution or release authority.
import { execFileSync } from "node:child_process";
import { developmentUpstream } from "./dev-server.mjs";

try {
  const upstream = developmentUpstream(process.env.RYEOS_UI_DEV_UPSTREAM);
  const development = new URL("http://127.0.0.1:7411");
  const response = await fetch(new URL("/ui", development), {
    signal: AbortSignal.timeout(3000), redirect: "error",
  });
  if (!response.ok || !(await response.text()).includes("/ui/assets/@vite/client")) {
    throw new Error("port 7411 is not the RyeOS Vite development server");
  }
  const output = execFileSync("ryeos", ["web", "--no-open", "--print-url"], {
    encoding: "utf8", timeout: 30000, maxBuffer: 32 * 1024,
    stdio: ["inherit", "pipe", "inherit"],
  }).trim();
  const launch = new URL(output);
  if (launch.origin !== upstream || !launch.pathname.startsWith("/ui/launch/")) {
    throw new Error("launch does not match RYEOS_UI_DEV_UPSTREAM; select the same node for the CLI and Vite");
  }
  launch.host = development.host;
  // Only the requested launch URL is printed. Never automatically commandeer
  // the operator's desktop browser or persist the one-time token to a file.
  console.log(launch.href);
} catch (error) {
  console.error(error instanceof Error ? error.message : "development launch failed");
  process.exitCode = 1;
}
