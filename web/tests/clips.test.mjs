// Saving what already happened: the button that reaches into the gateway's
// ring buffer rather than starting a new recording.
import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');
const brand = JSON.parse(read('brand.json'));
const dict = JSON.parse(read('locales/en.json'));
const demoFleet = JSON.parse(read('demo-fleet.json'));

const cameras = [
  { camera_id: 'cam-1', gateway_id: 'gw-live', site_id: 'site-1', name: 'Yard', status: 'healthy',
    fps: 25, bitrate_kbps: 2000, packet_loss: 0, reconnects: 0, codec: 'H264', width: 1920,
    height: 1080, last_seen: new Date().toISOString(), last_error: null,
    rtsp_endpoint: 'rtsp://10.0.0.7:554/stream' },
];
const roster = [
  { gateway_id: 'gw-live', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-live', version: '0.1.0', enrolled: true, revoked_at: null,
    last_seen: new Date().toISOString(), online: true, heartbeat: null },
];
// A live fleet with one camera, which is what puts a camera card on screen.
const fleet = {
  generated_at: new Date().toISOString(),
  source: 'live',
  customers: [{
    id: 'cust-1', name: 'Customer', sites: [{
      id: 'site-1', customer_id: 'cust-1', name: 'Site', city: 'Town',
      cameras: [{ id: 'cam-1', name: 'Yard', site_id: 'site-1', status: 'healthy',
        fps: 25, bitrate_kbps: 2000, last_seen: new Date().toISOString() }],
    }],
  }],
};
const plugins = [
  { reachable: true, manifest: { id: 'storage-s3', capabilities: ['storage_blob'] } },
];

let startDashboard, posted;

function stubFetch() {
  posted = [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if ((options.method || 'GET') === 'POST') {
      posted.push({ path, body: options.body ? JSON.parse(options.body) : null });
      return { ok: true, status: 202, json: async () => ({ command_id: 'cmd-1' }) };
    }
    const body =
      path.endsWith('api/v1/fleet') ? fleet
      : path.includes('/commands/') ? { status: 'succeeded', result: { recording: null } }
      : path.includes('api/v1/cameras') && path.includes('/recordings') ? { recordings: [] }
      : path.includes('api/v1/cameras') ? cameras
      : path.includes('api/v1/gateways') ? roster
      : path.includes('api/v1/plugins') ? plugins
      : path.includes('api/v1/sources') || path.includes('api/v1/incidents') ? []
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
  dom.window.confirm = () => true;
  globalThis.confirm = dom.window.confirm;
  globalThis.HTMLElement = dom.window.HTMLElement;
}

beforeEach(async () => {
  loadPage();
  stubFetch();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('saving the last minutes asks the gateway for a clip, not a new recording', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  // Open a site's cameras, which is where the per-camera buttons live.
  document.querySelector('#fleet-body [data-live]').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const button = document.querySelector('[data-save-clip]');
  assert.ok(button, 'every camera offers to keep what already happened');
  button.dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const clip = posted.find(entry => entry.path.includes('/clips'));
  assert.ok(clip, `expected a clip request, got ${JSON.stringify(posted)}`);
  assert.match(clip.path, /cameras\/cam-1\/clips/);
  assert.equal(typeof clip.body.seconds, 'number');
  assert.ok(clip.body.seconds > 0);
  assert.equal(clip.body.storage_plugin_id, 'storage-s3');
});

test('a camera can be set to record itself, on a schedule', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#fleet-body [data-live]').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const form = document.querySelector('[data-policy-form]');
  assert.ok(form, 'a camera says how it is recorded');
  form.querySelector('[name="mode"]').value = 'continuous';
  form.querySelector('[name="from"]').value = '08:00';
  form.querySelector('[name="to"]').value = '18:00';
  form.querySelector('[name="retention"]').value = '14';
  // Monday is bit 0, which is what the checkbox values are: bits, not
  // weekday numbers with Sunday somewhere.
  for (const day of form.querySelectorAll('[name="day"]')) {
    day.checked = ['0', '1', '2', '3', '4'].includes(day.value);
  }
  form.dispatchEvent(new Event('submit', { cancelable: true, bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const saved = posted.find(entry => entry.path.includes('recording-policy'));
  assert.ok(saved, `expected a policy post, got ${JSON.stringify(posted)}`);
  assert.equal(saved.body.mode, 'continuous');
  assert.equal(saved.body.retention_days, 14);
  assert.deepEqual(saved.body.keep, [
    { type: 'schedule', days: 0b0001_1111, from_minute: 480, to_minute: 1080 },
  ]);
});

test('a camera recorded around the clock keeps no schedule rule', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#fleet-body [data-live]').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const form = document.querySelector('[data-policy-form]');
  form.querySelector('[name="mode"]').value = 'continuous';
  form.querySelector('[name="from"]').value = '';
  form.querySelector('[name="to"]').value = '';
  form.dispatchEvent(new Event('submit', { cancelable: true, bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const saved = posted.find(entry => entry.path.includes('recording-policy'));
  assert.deepEqual(saved.body.keep, [], 'no window means the ring is kept for clipping only');
});

test('a camera offers a way to its own web page', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#fleet-body [data-live]').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const button = document.querySelector('[data-open-device]');
  assert.ok(button, 'a camera card offers the device page');
  globalThis.window.open = (url) => { globalThis.window.__opened = url; return null; };
  button.dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const opened = posted.find(entry => entry.path.includes('/tunnel'));
  assert.ok(opened, `expected a tunnel request, got ${JSON.stringify(posted)}`);
  assert.match(opened.path, /gateways\/gw-live\/tunnel/);
  assert.equal(typeof opened.body.host, 'string');
  assert.ok(opened.body.host.length > 0, 'the host comes from what the camera reported');
  assert.equal(opened.body.port, 80);
});
