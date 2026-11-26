// Drives the browser client headless from a minted launch URL and fails on the
// first broken step: the launch redirect, the session read, the seat claim, the
// rewritten resume address, a page error, a refused Content-Security-Policy
// load, any failing /ui/api request, or the launcher not opening on Ctrl+K.
//
// It also covers the drawn Surface ground: the session's config read carries a
// theme catalogue with Surface grounds; the page fetches the granted pictures
// from /ui/art/<sha256> and gets 200 with the media type the catalogue named;
// an installed picture the session was not granted, a hash that names nothing
// and a request with no session are all refused; and the canvas differs from
// the same page reloaded with every picture fetch refused, which draws flat.
//
//   node smoke.mjs <launch-url> <screenshot-dir>
//
// RYEOS_SMOKE_PICTURE_BLOBS names the installed picture blob directory (each
// file is named by its SHA-256, with an image extension).
//
// Playwright resolves from PLAYWRIGHT_ROOT (a node_modules directory) when set,
// else from this directory.
import { createRequire } from 'node:module';
import { mkdirSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

const [url, shots] = process.argv.slice(2);
if (!url || !shots) {
  console.error('usage: node smoke.mjs <launch-url> <screenshot-dir>');
  process.exit(2);
}
mkdirSync(shots, { recursive: true });
const require = createRequire(join(process.env.PLAYWRIGHT_ROOT || import.meta.dirname, 'node_modules') + '/');
const { chromium, request } = require('@playwright/test');

const failures = [];
const api = [];
const HASH = /^[0-9a-f]{64}$/;
const PICTURE_TYPES = ['image/webp', 'image/png', 'image/jpeg'];
// 'ground' until the pictures are refused on purpose, then 'flat'.
let phase = 'ground';
const art = { ground: [], flat: [] };
let catalogue = null;
const pending = [];
const browser = await chromium.launch({
  // A runner has no GPU; SwiftShader gives the page a WebGL2 context.
  args: ['--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
page.on('pageerror', (error) => failures.push(`page error: ${error.message}`));
page.on('console', (message) => {
  if (message.type() !== 'error') return;
  // The browser reports a refused picture fetch as a console error; the flat
  // phase refuses them on purpose.
  if (phase === 'flat' && message.location().url.includes('/ui/art/')) return;
  failures.push(`console error: ${message.text()}`);
});
page.on('requestfailed', (request) => {
  failures.push(`request failed: ${new URL(request.url()).pathname} ${request.failure()?.errorText}`);
});
function findKey(value, key) {
  if (!value || typeof value !== 'object') return undefined;
  if (key in value) return value[key];
  for (const child of Object.values(value)) {
    const found = findKey(child, key);
    if (found !== undefined) return found;
  }
  return undefined;
}
page.on('response', (response) => {
  const path = new URL(response.url()).pathname;
  if (path.startsWith('/ui/art/')) {
    art[phase].push({
      hash: path.slice('/ui/art/'.length),
      status: response.status(),
      type: (response.headers()['content-type'] || '').split(';')[0].trim(),
    });
  }
  // The session's own config read, as the page made it.
  if (path === '/ui/api/invocations/dispatch' && response.status() === 200) {
    pending.push(
      response
        .json()
        .then((body) => {
          const found = findKey(body, 'theme_catalogue');
          if (found && !catalogue) catalogue = found;
        })
        .catch(() => {}),
    );
  }
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

// The ground: the catalogue arrived, the page fetched the pictures it was
// granted, and the canvas has had time to decode and draw them.
await settle(async () => (await Promise.all(pending), catalogue !== null), 'theme catalogue in the config read');
await settle(async () => art.ground.length > 0, 'a /ui/art picture fetch');
await page.waitForTimeout(4000);
await page.screenshot({ path: join(shots, 'shell.png'), timeout: 90000 });

const groundHashes = new Map();
for (const theme of catalogue?.themes ?? []) {
  for (const [surface, ground] of Object.entries(theme.surface_grounds ?? {})) {
    if (ground.picture_state === 'unavailable') failures.push(`ground unavailable: ${theme.ref} ${surface}`);
    const picture = ground.picture;
    if (!picture) continue;
    if (!HASH.test(picture.sha256) || !PICTURE_TYPES.includes(picture.media_type)) {
      failures.push(`malformed ground picture: ${theme.ref} ${surface} ${JSON.stringify(picture)}`);
      continue;
    }
    groundHashes.set(picture.sha256, picture.media_type);
  }
}
if (catalogue && groundHashes.size === 0) failures.push('the theme catalogue carries no Surface ground');
if (catalogue && !(catalogue.themes ?? []).every((theme) => /^theme:/.test(theme.ref))) {
  failures.push('the theme catalogue names a theme with no theme: ref');
}
for (const { hash, status, type } of art.ground) {
  if (!groundHashes.has(hash)) failures.push(`fetched a picture the catalogue did not grant: ${hash}`);
  else if (status !== 200) failures.push(`/ui/art/${hash} answered ${status}`);
  else if (type !== groundHashes.get(hash)) failures.push(`/ui/art/${hash} is ${type}, catalogue says ${groundHashes.get(hash)}`);
}

// Refusals, with the page's own session. A picture installed but never
// granted, a hash that names nothing, and no session at all.
const blobDir = process.env.RYEOS_SMOKE_PICTURE_BLOBS;
if (!blobDir) failures.push('RYEOS_SMOKE_PICTURE_BLOBS is not set');
else {
  const installed = readdirSync(blobDir).map((name) => name.split('.')[0]).filter((name) => HASH.test(name));
  if (installed.length === 0) failures.push(`no picture blobs installed under ${blobDir}`);
  const origin = new URL(page.url()).origin;
  const ungranted = installed.filter((hash) => !groundHashes.has(hash));
  let refused = 0;
  for (const hash of ungranted) {
    const status = (await page.request.get(`${origin}/ui/art/${hash}`)).status();
    if (status === 404) refused += 1;
    else if (status !== 200) failures.push(`ungranted picture ${hash} answered ${status}, not 404`);
  }
  // A picture some compiled binding names is legitimately served; at least one
  // installed picture must be neither granted nor bound.
  if (refused === 0) failures.push('no installed picture was refused as ungranted');
  const unknown = (await page.request.get(`${origin}/ui/art/${'0'.repeat(64)}`)).status();
  if (unknown !== 404) failures.push(`an unknown hash answered ${unknown}, not 404`);
  const granted = [...groundHashes.keys()][0];
  if (granted) {
    const anonymous = await request.newContext();
    const status = (await anonymous.get(`${origin}/ui/art/${granted}`)).status();
    if (status === 200) failures.push('a granted picture was served with no session');
    await anonymous.dispose();
  }
}

await page.keyboard.press('Control+k');
// The launcher's list in the accessibility mirror: it opened, and assistive
// technology reaches it.
await settle(
  () => page.evaluate(() => !!document.querySelector('[data-rune-a11y] [aria-label="Launch"]')),
  'launcher opens on Ctrl+K and is mirrored',
  10000,
);
await page.screenshot({ path: join(shots, 'launcher.png'), timeout: 90000 });

// The same session reloaded with every picture fetch refused draws the Surface
// flat; the grounded canvas must differ from it.
phase = 'flat';
await page.route('**/ui/art/**', (route) => route.fulfill({ status: 404, body: 'refused' }));
await page.reload();
await settle(async () => (await page.evaluate(() => document.getElementById('ryeos-status')?.textContent || '')) === '', 'boot status cleared after reload');
await settle(async () => art.flat.length > 0, 'a refused /ui/art fetch after reload');
await page.waitForTimeout(4000);
if (art.flat.some((entry) => entry.status !== 404)) failures.push('a picture fetch escaped the refusal after reload');
const flatPng = await page.screenshot({ path: join(shots, 'flat.png'), timeout: 90000 });
const groundPng = readFileSync(join(shots, 'shell.png'));
const changed = await page.evaluate(async ([a, b]) => {
  const decode = async (base64) => {
    const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
    const bitmap = await createImageBitmap(new Blob([bytes], { type: 'image/png' }));
    const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
    const context = canvas.getContext('2d');
    context.drawImage(bitmap, 0, 0);
    return context.getImageData(0, 0, bitmap.width, bitmap.height);
  };
  const [one, two] = [await decode(a), await decode(b)];
  if (one.width !== two.width || one.height !== two.height) return { error: 'screenshot sizes differ' };
  let differing = 0;
  for (let i = 0; i < one.data.length; i += 4) {
    const delta = Math.max(
      Math.abs(one.data[i] - two.data[i]),
      Math.abs(one.data[i + 1] - two.data[i + 1]),
      Math.abs(one.data[i + 2] - two.data[i + 2]),
    );
    if (delta > 8) differing += 1;
  }
  return { fraction: differing / (one.width * one.height) };
}, [groundPng.toString('base64'), flatPng.toString('base64')]);
if (changed.error) failures.push(changed.error);
else {
  console.log(`ground differs from the flat Surface in ${(changed.fraction * 100).toFixed(1)}% of pixels`);
  if (changed.fraction < 0.03) failures.push(`the ground drew nothing visible: only ${(changed.fraction * 100).toFixed(2)}% of pixels differ from the flat Surface`);
}

await browser.close();
console.log(api.join('\n'));
if (failures.length) {
  console.error(`browser smoke failed:\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log('browser smoke passed');
