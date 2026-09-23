// Video sources: addresses the gateway pulls, added and removed from the dashboard.
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

const roster = [
  { gateway_id: 'gw-live', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-live', version: '0.1.0', enrolled: true, revoked_at: null,
    last_seen: new Date().toISOString(), online: true, heartbeat: null },
];
const sources = [
  { id: 'src-1', gateway_id: 'gw-live', name: 'Yard NVR', kind: 'rtsp',
    address: 'rtsp://10.0.0.7/stream1', added_at: new Date().toISOString() },
  { id: 'src-2', gateway_id: 'gw-live', name: 'Loading bay', kind: 'rtmp',
    address: 'loading-bay', added_at: new Date().toISOString() },
  { id: 'src-3', gateway_id: 'gw-live', name: 'Drone feed', kind: 'srt',
    address: 'drone', added_at: new Date().toISOString() },
];

let startDashboard, posted;

function stubFetch({ postStatus = 201 } = {}) {
  posted = [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if ((options.method || 'GET') === 'POST') {
      posted.push({ path, body: options.body ? JSON.parse(options.body) : null });
      return { ok: postStatus < 400, status: postStatus, json: async () => ({ id: 'src-new' }) };
    }
    const body =
      path.includes('api/v1/sources') ? sources
      : path.includes('api/v1/gateways') ? roster
      : path.includes('api/v1/incidents') || path.includes('api/v1/cameras') ? []
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

test('the sources a gateway carries are listed with their address', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const text = document.querySelector('#sources-list').textContent;
  assert.match(text, /Yard NVR/);
  assert.match(text, /rtsp:\/\/10\.0\.0\.7\/stream1/);
});

test('adding a source posts what the form holds', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const form = document.querySelector('#source-form');
  form.querySelector('[name="name"]').value = 'Gate camera';
  form.querySelector('[name="address"]').value = 'rtsp://10.0.0.9/stream';
  form.querySelector('[name="gatewayId"]').value = 'gw-live';
  form.dispatchEvent(new Event('submit', { cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(posted.length, 1, JSON.stringify(posted));
  assert.equal(posted[0].path, 'api/v1/sources');
  assert.deepEqual(posted[0].body, {
    gateway_id: 'gw-live', name: 'Gate camera', kind: 'rtsp', address: 'rtsp://10.0.0.9/stream',
  });
});

test('removing a source confirms and posts to its delete', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#sources-list .source-remove')
    .dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(posted.map(p => p.path), ['api/v1/sources/src-1/delete']);
});

test('a refused add says so instead of failing quietly', async () => {
  stubFetch({ postStatus: 422 });
  const alerts = [];
  globalThis.alert = message => alerts.push(message);
  await startDashboard({ brand, locale: 'en', dict });
  const form = document.querySelector('#source-form');
  form.querySelector('[name="name"]').value = 'Gate camera';
  form.querySelector('[name="address"]').value = 'rtsp://admin:pw@10.0.0.9/stream';
  form.querySelector('[name="gatewayId"]').value = 'gw-live';
  form.dispatchEvent(new Event('submit', { cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(alerts, [dict['app.sources.addFailed']]);
});

test('a pushed source says where to publish to it', async () => {
  // A stream key is useless on its own: whoever sets the encoder up needs the
  // URL, and it is built from the gateway this source belongs to.
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#sources-list .plugin-card')];
  const rtmp = cards.find(card => card.textContent.includes('Loading bay'));
  assert.match(rtmp.textContent, /rtmp:\/\/edge-live:1935\/live\/loading-bay/);
  const srt = cards.find(card => card.textContent.includes('Drone feed'));
  assert.match(srt.textContent, /srt:\/\/edge-live:9000\?streamid=drone/);
  const pulled = cards.find(card => card.textContent.includes('Yard NVR'));
  assert.doesNotMatch(pulled.textContent, /publish/i, 'a pulled source is dialled, not published to');
});
