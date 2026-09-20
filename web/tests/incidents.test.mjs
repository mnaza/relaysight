// The incidents view: real history rows, an ongoing badge, and a stat that
// counts open incidents instead of guessing from live status.
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

const anHourAgo = new Date(Date.now() - 3600_000).toISOString();
const twoIncidents = [
  { camera_id: 'cam-001', camera_name: 'Entrance', site_id: 'madrid-centro', site_name: 'Madrid Centro',
    started_at: anHourAgo, ended_at: null, detail: 'gateway telemetry is stale' },
  { camera_id: 'cam-002', camera_name: 'Checkout 01', site_id: 'madrid-centro', site_name: 'Madrid Centro',
    started_at: anHourAgo, ended_at: new Date(Date.now() - 3000_000).toISOString(), detail: null },
];

let startDashboard;
let incidentsBody;

function stubFetch() {
  globalThis.fetch = async (url) => {
    const path = String(url);
    const body =
      path.includes('api/v1/incidents') ? incidentsBody
      : path.endsWith('demo-fleet.json') ? demoFleet
      : path.endsWith('demo-plugins.json') ? []
      : undefined;
    if (body === undefined) return { ok: false, status: 404, json: async () => ({}) };
    return { ok: true, status: 200, json: async () => body };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'localStorage', 'URL', 'Event', 'FormData']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key];
  }
}

beforeEach(async () => {
  loadPage();
  incidentsBody = twoIncidents;
  stubFetch();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('the incidents table renders history with an ongoing badge and a cause', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const rows = document.querySelectorAll('#incidents-body tr');
  assert.equal(rows.length, 2);
  assert.match(rows[0].textContent, /Entrance/);
  assert.match(rows[0].textContent, /Ongoing/);
  assert.match(rows[0].textContent, /gateway telemetry is stale/);
  assert.match(rows[1].textContent, /min/, 'a closed incident shows a duration');
  assert.equal(document.querySelector('#incidents-empty').hidden, true);
});

test('the open-incidents stat counts open incidents, not live camera status', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  assert.equal(document.querySelector('#stat-alerts').textContent, '1');
});

test('no incidents shows the empty state and a zero stat', async () => {
  incidentsBody = [];
  await startDashboard({ brand, locale: 'en', dict });
  assert.equal(document.querySelectorAll('#incidents-body tr').length, 0);
  assert.equal(document.querySelector('#incidents-empty').hidden, false);
  assert.equal(document.querySelector('#stat-alerts').textContent, '0');
});

test('the incidents nav link is enabled and points at the view', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const link = document.querySelector('a[href="#incidents"]');
  assert.ok(link, 'no nav link to #incidents');
  assert.ok(!link.classList.contains('disabled'));
});
