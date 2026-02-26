// rye:signed:2026-02-26T05:52:24Z:86f089cf310d3ff22be3f65d2768cea1e3c568955b4276605c3911d67bdd8b9e:Y8FK_drmVbfav1tsSziNRQbBgm4YY3Yi1P_U6zK3ldd6OPzZfLL5K69blImk-PNhx5SSmec9uqsKwk7eafdNAA==:4b987fd4e40303ac
// rye:unsigned
import { parseArgs } from "node:util";
import { spawn } from "node:child_process";
import { resolve, isAbsolute, extname, relative } from "node:path";
import { existsSync, readFileSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
import { execSync } from "node:child_process";
import {
  createMessageConnection,
  StreamMessageReader,
  StreamMessageWriter,
} from "vscode-jsonrpc/node.js";

export const __version__ = "1.0.0";
export const __tool_type__ = "javascript";
export const __executor_id__ = "rye/core/runtimes/node/node";
export const __category__ = "rye/code/lsp";
export const __tool_description__ =
  "LSP client — go to definition, find references, hover, document symbols, and more via language servers";

const OPERATIONS = [
  "goToDefinition",
  "findReferences",
  "hover",
  "documentSymbol",
  "workspaceSymbol",
  "goToImplementation",
  "prepareCallHierarchy",
  "incomingCalls",
  "outgoingCalls",
] as const;

export const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    operation: {
      type: "string",
      enum: [...OPERATIONS],
      description: "LSP operation to perform",
    },
    file_path: {
      type: "string",
      description: "Path to the file",
    },
    line: {
      type: "integer",
      minimum: 1,
      description: "Line number (1-based)",
    },
    character: {
      type: "integer",
      minimum: 1,
      description: "Character offset (1-based)",
    },
    timeout: {
      type: "integer",
      default: 15,
      description: "Timeout in seconds",
    },
  },
  required: ["operation", "file_path", "line", "character"],
};

const DEFAULT_TIMEOUT = 15;

const LANGUAGE_EXTENSIONS: Record<string, string> = {
  ".py": "python",
  ".js": "javascript",
  ".jsx": "javascriptreact",
  ".ts": "typescript",
  ".tsx": "typescriptreact",
  ".mjs": "javascript",
  ".cjs": "javascript",
  ".mts": "typescript",
  ".cts": "typescript",
  ".go": "go",
  ".rs": "rust",
  ".rb": "ruby",
  ".java": "java",
  ".c": "c",
  ".cpp": "cpp",
  ".h": "c",
  ".hpp": "cpp",
  ".vue": "vue",
  ".svelte": "svelte",
};

interface ServerConfig {
  id: string;
  extensions: string[];
  command: string[];
}

const SERVERS: ServerConfig[] = [
  {
    id: "typescript",
    extensions: [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
    command: ["typescript-language-server", "--stdio"],
  },
  {
    id: "pyright",
    extensions: [".py"],
    command: ["pyright-langserver", "--stdio"],
  },
  {
    id: "gopls",
    extensions: [".go"],
    command: ["gopls", "serve"],
  },
  {
    id: "rust-analyzer",
    extensions: [".rs"],
    command: ["rust-analyzer"],
  },
];

interface Params {
  operation: (typeof OPERATIONS)[number];
  file_path: string;
  line: number;
  character: number;
  timeout?: number;
}

interface Result {
  success: boolean;
  output?: string;
  error?: string;
  operation?: string;
  server?: string;
  results?: unknown[];
}

function findServer(filePath: string): ServerConfig | null {
  const ext = extname(filePath).toLowerCase();
  for (const server of SERVERS) {
    if (!server.extensions.includes(ext)) continue;
    try {
      execSync(`which ${server.command[0]}`, {
        encoding: "utf-8",
        stdio: ["pipe", "pipe", "pipe"],
      });
      return server;
    } catch {
      continue;
    }
  }
  return null;
}

function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`Timed out after ${ms}ms`)),
      ms,
    );
    promise.then(
      (v) => {
        clearTimeout(timer);
        resolve(v);
      },
      (e) => {
        clearTimeout(timer);
        reject(e);
      },
    );
  });
}

async function execute(params: Params, projectPath: string): Promise<Result> {
  const project = resolve(projectPath);
  const operation = params.operation;
  const timeout = (params.timeout ?? DEFAULT_TIMEOUT) * 1000;

  let filePath = params.file_path;
  if (!isAbsolute(filePath)) filePath = resolve(project, filePath);

  if (!existsSync(filePath)) {
    return { success: false, error: `File not found: ${filePath}` };
  }

  const server = findServer(filePath);
  if (!server) {
    return {
      success: false,
      error: `No LSP server available for ${extname(filePath)} files`,
    };
  }

  const proc = spawn(server.command[0], server.command.slice(1), {
    cwd: project,
    stdio: ["pipe", "pipe", "pipe"],
  });

  const connection = createMessageConnection(
    new StreamMessageReader(proc.stdout),
    new StreamMessageWriter(proc.stdin),
  );

  // Suppress unhandled notifications
  connection.onRequest("window/workDoneProgress/create", () => null);
  connection.onRequest("workspace/configuration", () => [{}]);
  connection.onRequest("client/registerCapability", () => {});
  connection.onRequest("client/unregisterCapability", () => {});
  connection.onRequest("workspace/workspaceFolders", () => [
    { name: "workspace", uri: pathToFileURL(project).href },
  ]);

  connection.listen();

  try {
    // Initialize
    await withTimeout(
      connection.sendRequest("initialize", {
        rootUri: pathToFileURL(project).href,
        processId: proc.pid,
        workspaceFolders: [
          { name: "workspace", uri: pathToFileURL(project).href },
        ],
        capabilities: {
          workspace: {
            configuration: true,
            didChangeWatchedFiles: { dynamicRegistration: true },
          },
          textDocument: {
            synchronization: { didOpen: true, didChange: true },
            publishDiagnostics: { versionSupport: true },
          },
        },
      }),
      timeout,
    );

    await connection.sendNotification("initialized", {});

    // Open file
    const ext = extname(filePath);
    const languageId = LANGUAGE_EXTENSIONS[ext] ?? "plaintext";
    const text = readFileSync(filePath, "utf-8");
    const uri = pathToFileURL(filePath).href;

    await connection.sendNotification("textDocument/didOpen", {
      textDocument: { uri, languageId, version: 0, text },
    });

    // Brief delay for server to process
    await new Promise((r) => setTimeout(r, 500));

    const position = {
      line: params.line - 1,
      character: params.character - 1,
    };
    const textDocument = { uri };

    // Execute operation
    let results: unknown[];
    switch (operation) {
      case "goToDefinition":
        results = await withTimeout(
          connection
            .sendRequest("textDocument/definition", { textDocument, position })
            .then(normalize),
          timeout,
        );
        break;
      case "findReferences":
        results = await withTimeout(
          connection
            .sendRequest("textDocument/references", {
              textDocument,
              position,
              context: { includeDeclaration: true },
            })
            .then(normalize),
          timeout,
        );
        break;
      case "hover":
        results = await withTimeout(
          connection
            .sendRequest("textDocument/hover", { textDocument, position })
            .then((r) => (r ? [r] : [])),
          timeout,
        );
        break;
      case "documentSymbol":
        results = await withTimeout(
          connection
            .sendRequest("textDocument/documentSymbol", { textDocument })
            .then(normalize),
          timeout,
        );
        break;
      case "workspaceSymbol":
        results = await withTimeout(
          connection
            .sendRequest("workspace/symbol", { query: "" })
            .then(normalize)
            .then((r: any[]) => r.slice(0, 20)),
          timeout,
        );
        break;
      case "goToImplementation":
        results = await withTimeout(
          connection
            .sendRequest("textDocument/implementation", {
              textDocument,
              position,
            })
            .then(normalize),
          timeout,
        );
        break;
      case "prepareCallHierarchy":
        results = await withTimeout(
          connection
            .sendRequest("textDocument/prepareCallHierarchy", {
              textDocument,
              position,
            })
            .then(normalize),
          timeout,
        );
        break;
      case "incomingCalls": {
        const items: any[] = await withTimeout(
          connection
            .sendRequest("textDocument/prepareCallHierarchy", {
              textDocument,
              position,
            })
            .catch(() => []),
          timeout,
        );
        if (!items?.length) {
          results = [];
        } else {
          results = await withTimeout(
            connection
              .sendRequest("callHierarchy/incomingCalls", { item: items[0] })
              .then(normalize),
            timeout,
          );
        }
        break;
      }
      case "outgoingCalls": {
        const items2: any[] = await withTimeout(
          connection
            .sendRequest("textDocument/prepareCallHierarchy", {
              textDocument,
              position,
            })
            .catch(() => []),
          timeout,
        );
        if (!items2?.length) {
          results = [];
        } else {
          results = await withTimeout(
            connection
              .sendRequest("callHierarchy/outgoingCalls", { item: items2[0] })
              .then(normalize),
            timeout,
          );
        }
        break;
      }
      default:
        return { success: false, error: `Unknown operation: ${operation}` };
    }

    // Convert file URIs to relative paths
    const cleaned = JSON.parse(
      JSON.stringify(results, (key, value) => {
        if (
          key === "uri" &&
          typeof value === "string" &&
          value.startsWith("file://")
        ) {
          try {
            return relative(project, fileURLToPath(value));
          } catch {
            return value;
          }
        }
        return value;
      }),
    );

    const output =
      cleaned.length === 0
        ? `No results found for ${operation}`
        : JSON.stringify(cleaned, null, 2);

    const relPath = relative(project, filePath);
    return {
      success: true,
      output,
      operation,
      server: server.id,
      results: cleaned,
    };
  } catch (e: any) {
    return {
      success: false,
      error: e.message ?? String(e),
      operation,
      server: server.id,
    };
  } finally {
    connection.end();
    connection.dispose();
    proc.kill();
  }
}

function normalize(result: unknown): unknown[] {
  if (!result) return [];
  return Array.isArray(result) ? result.filter(Boolean) : [result];
}

// CLI entry point
const { values } = parseArgs({
  options: {
    params: { type: "string" },
    "project-path": { type: "string" },
  },
});

if (values.params && values["project-path"]) {
  const result = await execute(
    JSON.parse(values.params) as Params,
    values["project-path"],
  );
  console.log(JSON.stringify(result));
}
