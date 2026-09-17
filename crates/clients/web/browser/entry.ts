// Installed browser entrypoint. The HTML document may reference this signed
// external module under the node's strict CSP; executable inline markup is
// deliberately forbidden.
export * from "./main";

import { bootRyeOsDocument } from "./runtime/boot";

void bootRyeOsDocument();
