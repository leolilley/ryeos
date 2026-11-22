// Drives the browser client headless from a minted launch URL and fails on the
// first broken step: the launch redirect, the session read, the seat claim, the
// rewritten resume address, a page error, a refused Content-Security-Policy
// load, any failing /ui/api request, or the launcher not opening on Ctrl+K.
//
//   node smoke.mjs <launch-url> <screenshot-dir>
//
// Playwright resolves from PLAYWRIGHT_ROOT (a node_modules directory) when set,
// else from this directory.
import { createRequire } from 'node:module';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';

const [url, shots] = process.argv.slice(2);
if (!url || !shots) {
  console.error('usage: node smoke.mjs <launch-url> <screenshot-dir>');
  process.exit(2);
}
mkdirSync(shots, { recursive: true });
const require = createRequire(join(process.env.PLAYWRIGHT_ROOT || import.meta.dirname, 'node_modules') + '/');
const { chromium } = require('@playwright/test');

const failures = [];
const api = [];
const browser = await chromium.launch({
  // A runner has no GPU; SwiftShader gives the page a WebGL2 context.
  args: ['--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
page.on('pageerror', (error) => failures.push(`page error: ${error.message}`));
page.on('console', (message) => {
  if (message.type() === 'error') failures.push(`console error: ${message.text()}`);
});
page.on('requestfailed', (request) => {
  failures.push(`request failed: ${new URL(request.url()).pathname} ${request.failure()?.errorText}`);
});
page.on('response', (response) => {
  const path = new URL(response.url()).pathname;
  if (path.startsWith('/ui/api/')) {
    api.push(`${response.status()} ${response.request().method()} ${path}`);
    if (response.status() >= 400) failures.push(`api ${response.status()} ${path}`);
  }
});
await page.addInitScript(() => {
  document.addEventListener('securitypolicyviolation', (event) => {
    console.error(`CSP violation: ${event.violatedDirective} ${event.blockedURI}`);
  });
});

async function settle(predicate, label, ms = 30000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (await predicate()) return true;
    await page.waitForTimeout(250);
  }
  failures.push(`timed out: ${label}`);
  return false;
}

await page.goto(url);
await settle(async () => api.some((line) => line.startsWith('200 POST /ui/api/session/seat/open')), 'seat open');
await settle(async () => new URL(page.url()).searchParams.has('presentation_id'), 'resume address');
await settle(async () => (await page.evaluate(() => document.getElementById('ryeos-status')?.textContent || '')) === '', 'boot status cleared');
await page.waitForTimeout(1500);
await page.screenshot({ path: join(shots, 'shell.png'), timeout: 90000 });

await page.keyboard.press('Control+k');
// The launcher's list in the accessibility mirror: it opened, and assistive
// technology reaches it.
await settle(
  () => page.evaluate(() => !!document.querySelector('[data-rune-a11y] [aria-label="Launch"]')),
  'launcher opens on Ctrl+K and is mirrored',
  10000,
);
await page.screenshot({ path: join(shots, 'launcher.png'), timeout: 90000 });

await browser.close();
console.log(api.join('\n'));
if (failures.length) {
  console.error(`browser smoke failed:\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log('browser smoke passed');
