import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
const input = await readStdin();
const request = JSON.parse(input || '{}');
function respond(value) { process.stdout.write(JSON.stringify(value)); }
function artifact(kind, filePath) { return { kind, path: filePath }; }
try {
  const require = createRequire(import.meta.url);
  const playwright = require(request.playwright_package || 'playwright');
  const browserName = request.browser || 'chromium';
  const browserType = playwright[browserName];
  if (!browserType) throw new Error(`unknown Playwright browser '${browserName}'`);
  fs.mkdirSync(request.session_dir, { recursive: true });
  fs.mkdirSync(request.artifact_dir, { recursive: true });
  const context = await browserType.launchPersistentContext(request.session_dir, { headless: request.headless !== false, channel: request.channel || undefined });
  try {
    const params = request.params || {};
    const page = await getPage(context, request.session_dir);
    page.setDefaultTimeout(request.timeout_ms || 30000);
    if (params.action === 'navigate') {
      await page.goto(params.url, { waitUntil: 'domcontentloaded' });
      writeLastUrl(request.session_dir, page.url());
      respond({ success: true, output: `navigated to ${page.url()}`, artifacts: [] });
    } else if (params.action === 'screenshot') {
      const file = path.join(request.artifact_dir, 'screenshot.png');
      await page.screenshot({ path: file, fullPage: true });
      writeLastUrl(request.session_dir, page.url());
      respond({ success: true, output: `saved screenshot to ${file}`, artifacts: [artifact('screenshot', file)] });
    } else if (params.action === 'snapshot') {
      const snapshot = await ariaSnapshot(page);
      const file = path.join(request.artifact_dir, 'snapshot.txt');
      fs.writeFileSync(file, snapshot);
      writeLastUrl(request.session_dir, page.url());
      respond({ success: true, output: snapshot, artifacts: [artifact('snapshot', file)] });
    } else if (params.action === 'click') {
      await page.locator(params.selector).click();
      writeLastUrl(request.session_dir, page.url());
      respond({ success: true, output: `clicked ${params.selector}`, artifacts: [] });
    } else if (params.action === 'type') {
      await page.locator(params.selector).fill(params.text || '');
      writeLastUrl(request.session_dir, page.url());
      respond({ success: true, output: `typed into ${params.selector}`, artifacts: [] });
    } else {
      throw new Error(`unsupported action '${params.action}'`);
    }
  } finally { await context.close(); }
} catch (error) { respond({ success: false, error: error && error.stack ? error.stack : String(error), artifacts: [] }); }
async function getPage(context, sessionDir) { const page = context.pages()[0] || await context.newPage(); if (page.url() === 'about:blank') { const lastUrl = readLastUrl(sessionDir); if (lastUrl) await page.goto(lastUrl, { waitUntil: 'domcontentloaded' }); } return page; }
async function ariaSnapshot(page) { const body = page.locator('body'); if (typeof body.ariaSnapshot === 'function') return await body.ariaSnapshot(); return await body.innerText(); }
function lastUrlPath(sessionDir) { return path.join(sessionDir, '.ryeos-last-url'); }
function readLastUrl(sessionDir) { try { return fs.readFileSync(lastUrlPath(sessionDir), 'utf8').trim(); } catch { return ''; } }
function writeLastUrl(sessionDir, url) { if (url && url !== 'about:blank') fs.writeFileSync(lastUrlPath(sessionDir), url); }
async function readStdin() { const chunks = []; for await (const chunk of process.stdin) chunks.push(chunk); return Buffer.concat(chunks).toString('utf8'); }
