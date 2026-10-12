/// <reference types="vite/client" />
// Installed browser entrypoint. The HTML document may reference this signed
// external module under the node's strict CSP; executable inline markup is
// deliberately forbidden.
export * from "./main";

import { bootRyeOsDocument } from "./runtime/boot";
import { createDocumentLifetime } from "./runtime/document-lifetime";

// Vite's existing HMR data carries only a mechanical teardown barrier. It is
// not a session, seat, producer or browser-owned application state store.
const predecessorSettlement = import.meta.hot?.data.ryeosDocumentSettlement as Promise<void> | undefined;
const lifetime = createDocumentLifetime((signal) => bootRyeOsDocument(signal, predecessorSettlement));
// Boot owns visible diagnostics. Do not also leave an unhandled rejection.
void lifetime.ready.catch(() => {});
void lifetime.settled.catch((error: unknown) => console.error("RyeOS document settlement failed", error));
if (import.meta.hot) {
  // Component/CSS HMR retains this runtime. Replacing the entry itself uses
  // Vite's ordinary document reload; do not self-accept and claim a new writer.
  import.meta.hot.dispose((data) => {
    void lifetime.dispose().catch((error: unknown) => console.error("RyeOS document disposal failed", error));
    data.ryeosDocumentSettlement = lifetime.settled;
  });
}
