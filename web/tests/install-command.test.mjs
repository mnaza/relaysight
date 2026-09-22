// The command the dashboard hands an installer for a new gateway: the signed
// installer, never a camera password on a command line.
import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');
const shippedBrand = JSON.parse(read('brand.json'));
const dict = JSON.parse(read('locales/en.json'));
const demoFleet = JSON.parse(read('demo-fleet.json'));

let startDashboard;

function stubFetch() {
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if ((options.method || 'GET') === 'POST' && path.endsWith('api/v1/enrollments')) {
      return {
        ok: true, status: 200,
        json: async () => ({ enrollment_token: 'TOKEN123', expires_at: new Date(Date.now() + 1800_000).toISOString() }),
      };
    }
    const body =
      path.includes('api/v1/gateways') || path.includes('api/v1/cameras') || path.includes('api/v1/incidents') ? []
      : path.endsWith('demo-fleet.json') ? demoFleet
      : path.endsWith('demo-plugins.json') ? []
      : undefined;
    if (body === undefined) return { ok: false, status: 404, json: async () => ({}) };
    return { ok: true, status: 200, json: async () => body };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'Event', 'FormData']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key];
  }
}

beforeEach(async () => {
  loadPage();
  stubFetch();
  // Submitting enrolment starts a 2.5 s status poll; a real interval would keep the
  // test process alive for ever.
  globalThis.setInterval = () => 0;
  globalThis.clearInterval = () => {};
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

async function installCommandFor(brand) {
  await startDashboard({ brand, locale: 'en', dict });
  const form = document.querySelector('#enrollment-form');
  form.querySelector('[name="customerName"]').value = 'Acme';
  form.querySelector('[name="siteName"]').value = 'Main Street';
  form.dispatchEvent(new Event('submit', { cancelable: true }));
  // The node holds a placeholder until the enrolment answers.
  const node = document.querySelector('#install-command');
  for (let i = 0; i < 100 && !node.textContent.includes('TOKEN123'); i++) {
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  return document.querySelector('#install-command').textContent;
}

function assertInstallerCommand(command) {
  assert.match(command, /install\.sh/, `not the installer: ${command}`);
  assert.match(command, /\| sudo sh -s --/, command);
  assert.match(command, /--enrollment-token 'TOKEN123'/, command);
  assert.match(command, /--gateway-id 'gw-main-street-/, command);
  assert.match(command, /--api-url '/, command);
  assert.doesNotMatch(command, /CAMERA_PASSWORD|docker run/, 'a camera password has no business in a command line');
}

test('the shipped brand hands out the signed installer', async () => {
  assertInstallerCommand(await installCommandFor(shippedBrand));
});

test('a brand with no template of its own gets the installer too', async () => {
  const brand = structuredClone(shippedBrand);
  delete brand.gateway.installCommandTemplate;
  assertInstallerCommand(await installCommandFor(brand));
});
