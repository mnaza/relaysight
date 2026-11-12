// Alerts: what left the building, and whether it arrived.
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

const events = [
  {
    id: 'evt-1', kind: 'camera_offline', severity: 'critical',
    occurred_at: new Date().toISOString(), customer_id: 'cust-1', site_id: 'site-1',
    site_name: 'Bakery', gateway_id: 'gw-1', camera_id: 'cam-1',
    title: 'Yard camera stopped answering at Bakery', detail: 'RTSP probe failed',
    metadata: {},
    deliveries: [
      { plugin_id: 'webhook', attempts: 1, delivered_at: new Date().toISOString(),
        declined: false, last_error: null, next_attempt_at: null },
    ],
  },
  {
    id: 'evt-2', kind: 'gateway_offline', severity: 'critical',
    occurred_at: new Date().toISOString(), customer_id: 'cust-1', site_id: 'site-1',
    site_name: 'Bakery', gateway_id: 'gw-1', camera_id: null,
    title: 'Gateway edge-1 stopped reporting at Bakery', detail: null, metadata: {},
    deliveries: [
      { plugin_id: 'webhook', attempts: 3, delivered_at: null, declined: false,
        last_error: 'connection refused', next_attempt_at: null },
    ],
  },
  // A transition recorded while the camera was flapping: shown, not sent.
  {
    id: 'evt-held', kind: 'camera_offline', severity: 'critical',
    occurred_at: new Date().toISOString(), customer_id: 'cust-1', site_id: 'site-1',
    site_name: 'Bakery', gateway_id: 'gw-1', camera_id: 'cam-1',
    title: 'Yard camera stopped answering at Bakery', detail: 'RTSP probe failed',
    metadata: { gateway_id: 'gw-1', held: 'flapping' },
    deliveries: [],
  },
  {
    id: 'evt-settled', kind: 'camera_settled', severity: 'critical',
    occurred_at: new Date().toISOString(), customer_id: 'cust-1', site_id: 'site-1',
    site_name: 'Bakery', gateway_id: 'gw-1', camera_id: 'cam-1',
    title: 'Yard camera settled and is still down at Bakery',
    detail: '7 changes over 38 minutes; down since 21:52 UTC',
    metadata: { gateway_id: 'gw-1', changes: 7, state: 'down' },
    deliveries: [
      { plugin_id: 'webhook', attempts: 1, delivered_at: new Date().toISOString(),
        declined: false, last_error: null, next_attempt_at: null },
    ],
  },
];

let startDashboard, posted;

function stubFetch() {
  posted = [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if ((options.method || 'GET') === 'POST') {
      posted.push({ path, body: options.body ? JSON.parse(options.body) : null });
      return { ok: true, status: 200, json: async () => ({ event_id: 'evt-3', sinks: 1 }) };
    }
    const body =
      path.includes('api/v1/events') ? events
      : path.includes('api/v1/incidents') || path.includes('api/v1/cameras')
        || path.includes('api/v1/sources') || path.includes('api/v1/plugins') ? []
      : path.includes('api/v1/gateways') ? []
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

test('the alerts panel shows what was raised and where it got to', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const text = document.querySelector('#alerts-body').textContent;
  assert.match(text, /Yard camera stopped answering/);
  assert.match(text, /Gateway edge-1 stopped reporting/);
  assert.match(text, /webhook/, 'the sink is named');
});

test('an alert nobody could deliver says so, with the reason', async () => {
  // This is the case an operator has to see: silence that means nobody was
  // listening rather than nothing happened.
  await startDashboard({ brand, locale: 'en', dict });
  const rows = [...document.querySelectorAll('#alerts-body tr')];
  const failed = rows.find(row => row.textContent.includes('Gateway edge-1'));
  assert.match(failed.textContent, /connection refused/);
  const delivered = rows.find(row => row.textContent.includes('Yard camera'));
  assert.doesNotMatch(delivered.textContent, /connection refused/);
});

test('a transition held while the camera flaps says so, not "no sink registered"', async () => {
  // The dashboard keeps showing every transition; only the way out collapses.
  // A held row must not read as the silence that means nobody was listening.
  await startDashboard({ brand, locale: 'en', dict });
  const rows = [...document.querySelectorAll('#alerts-body tr')];
  const held = rows.find(row => row.textContent.includes('stopped answering') && row.textContent.includes('held'));
  assert.ok(held, 'the held transition is shown');
  assert.match(held.textContent, /held: the camera is flapping/);
  assert.doesNotMatch(held.textContent, /no event_sink plugin registered/);
  const settled = rows.find(row => row.textContent.includes('settled and is still down'));
  assert.ok(settled.querySelector('.alert-critical'), 'a camera that settled down is shown as critical');
});

test('the test button asks the control plane to send one', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#send-test-alert').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));
  assert.deepEqual(posted.map(entry => entry.path), ['api/v1/events/test']);
});
