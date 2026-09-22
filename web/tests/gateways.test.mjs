// The gateways grid: the whole roster, a revoked badge, and a revoke button
// that actually calls the API.
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

const heartbeat = {
  gateway_id: 'gw-live', site_id: 'site-1', hostname: 'edge-live', version: '0.1.0',
  uptime_seconds: 3600, cpu_percent: 5, memory_percent: 10, cameras_seen: 2,
  healthy_cameras: 2, warning_cameras: 0, offline_cameras: 0,
  sent_at: new Date().toISOString(),
};
const roster = [
  { gateway_id: 'gw-live', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-live', version: '0.1.0', enrolled: true, revoked_at: null,
    last_seen: heartbeat.sent_at, online: true, heartbeat },
  { gateway_id: 'gw-quiet', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-quiet', version: '0.1.0', enrolled: true, revoked_at: null,
    last_seen: new Date(Date.now() - 86400_000).toISOString(), online: false, heartbeat: null },
  { gateway_id: 'gw-dead', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-dead', version: '0.1.0', enrolled: false,
    revoked_at: new Date().toISOString(), last_seen: null, online: false, heartbeat: null },
];

let startDashboard;
let posted;

function stubFetch({ postStatus = 204 } = {}) {
  posted = [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if ((options.method || 'GET') === 'POST') {
      posted.push(path);
      return { ok: postStatus < 400, status: postStatus, json: async () => ({}) };
    }
    const body =
      path.includes('api/v1/gateways') ? roster
      : path.includes('api/v1/incidents') ? []
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

test('every roster gateway renders, not just the loud ones', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = document.querySelectorAll('#gateways-grid article');
  assert.equal(cards.length, 3);
  const text = document.querySelector('#gateways-grid').textContent;
  assert.match(text, /edge-quiet/, 'the enrolled-but-silent gateway vanished');
});

test('a revoked gateway shows the badge and no revoke button', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const dead = cards.find(card => card.textContent.includes('edge-dead'));
  assert.match(dead.textContent, /Revoked/);
  assert.equal(dead.querySelector('.gw-revoke'), null, 'a revoked gateway cannot be revoked again');
});

test('the revoke button confirms and posts to the revoke endpoint', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const quiet = cards.find(card => card.textContent.includes('edge-quiet'));
  quiet.querySelector('.gw-revoke').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(posted, ['api/v1/gateways/gw-quiet/revoke']);
});

test('a revoke the API refuses says so instead of failing silently', async () => {
  stubFetch({ postStatus: 500 });
  const alerts = [];
  globalThis.alert = message => alerts.push(message);
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const quiet = cards.find(card => card.textContent.includes('edge-quiet'));
  quiet.querySelector('.gw-revoke').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(alerts, [dict['app.gateways.revokeFailed']], 'the installer believed a failed revoke worked');
});

test('only a revoked gateway offers to retire its cameras', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const live = cards.find(card => card.textContent.includes('edge-live'));
  const dead = cards.find(card => card.textContent.includes('edge-dead'));
  assert.equal(live.querySelector('.gw-retire'), null, 'a working gateway keeps its cameras');
  assert.ok(dead.querySelector('.gw-retire'), 'a revoked gateway has no other way to tidy up');
});

test('retiring cameras confirms and posts to the retire endpoint', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const dead = cards.find(card => card.textContent.includes('edge-dead'));
  dead.querySelector('.gw-retire').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(posted, ['api/v1/gateways/gw-dead/cameras/retire']);
});

test('a retire the API refuses says so instead of failing silently', async () => {
  stubFetch({ postStatus: 409 });
  const alerts = [];
  globalThis.alert = message => alerts.push(message);
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const dead = cards.find(card => card.textContent.includes('edge-dead'));
  dead.querySelector('.gw-retire').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(alerts, [dict['app.gateways.retireFailed']], 'the installer believed a failed retire worked');
});
