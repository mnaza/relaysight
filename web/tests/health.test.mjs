// Health history on the screen: numbers that came from rollups, and an honest
// blank where there is no history at all.
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
const fleetHealth = {
  days: 7, healthy_seconds: 597_600, warning_seconds: 0, offline_seconds: 6_000,
  counted_seconds: 603_600, reconnects: 4, uptime_percent: 99.0, covered_percent: 99.8,
  hours: [],
};
const cameraHealth = {
  days: 7, healthy_seconds: 300_000, warning_seconds: 0, offline_seconds: 300_000,
  counted_seconds: 600_000, reconnects: 12, uptime_percent: 50.0, covered_percent: 99.2,
  hours: [],
};

let startDashboard, requested;

function stubFetch({ camera = cameraHealth } = {}) {
  requested = [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    requested.push(path);
    if ((options.method || 'GET') === 'POST') {
      return { ok: true, status: 200, json: async () => ({}) };
    }
    const body =
      path.includes('cameras/') && path.includes('/health') ? camera
      : path.startsWith('api/v1/health') ? fleetHealth
      : path.endsWith('api/v1/fleet') ? fleet
      : path.includes('api/v1/cameras') && path.includes('/recordings') ? { recordings: [] }
      : path.includes('api/v1/cameras') ? []
      : path.includes('api/v1/gateways') || path.includes('api/v1/plugins')
        || path.includes('api/v1/sources') || path.includes('api/v1/incidents')
        || path.includes('api/v1/events') ? []
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
}

beforeEach(async () => {
  loadPage();
  stubFetch();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('the overview shows uptime that came from the rollups', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  assert.match(document.querySelector('#stat-uptime').textContent, /99/);
  // And says how much of the week it actually heard about, because a number
  // over a day nobody reported would not be uptime.
  assert.match(document.querySelector('#sub-uptime').textContent, /99\.8/);
  assert.ok(requested.some(path => path.startsWith('api/v1/health')));
});

test('a camera carries its own week', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#fleet-body [data-live]').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));
  const card = document.querySelector('[data-camera-health]');
  assert.ok(card, 'the camera card says how its week went');
  assert.match(card.textContent, /50/);
  assert.match(card.textContent, /12/, 'and how many times it came back');
});

test('a camera with no history says so rather than claiming a perfect week', async () => {
  stubFetch({ camera: { days: 7, counted_seconds: 0, uptime_percent: null,
                        covered_percent: 0, reconnects: 0, hours: [] } });
  await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#fleet-body [data-live]').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));
  const card = document.querySelector('[data-camera-health]');
  assert.doesNotMatch(card.textContent, /100/);
  assert.match(card.textContent, new RegExp(dict['app.health.none']));
});
