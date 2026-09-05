// The dashboard, rendered.
//
// This is what the split was for. `startDashboard` takes its context as an
// argument, so a test can hand it a document, a brand and a dictionary and then
// look at what appeared on the page — none of which was possible while the file
// awaited loadRuntime() at module scope and read module bindings.
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

let startDashboard;
let requested;

/** Serve the API from fixtures; anything unexpected 404s so it is visible. */
function stubFetch(overrides = {}) {
  requested = [];
  globalThis.fetch = async (url) => {
    const path = String(url);
    requested.push(path);
    const body =
      overrides[path] ??
      (path.endsWith('demo-fleet.json') ? demoFleet
        : path.endsWith('demo-plugins.json') ? JSON.parse(read('demo-plugins.json'))
        : undefined);
    if (body === undefined) {
      return { ok: false, status: 404, json: async () => ({}) };
    }
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
  return dom;
}

beforeEach(async () => {
  loadPage();
  stubFetch();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('the shell is filled in from the brand and the dictionary', async () => {
  await startDashboard({ brand, locale: 'en', dict });

  assert.equal(document.documentElement.lang, 'en');
  assert.match(document.querySelector('#app-brand').textContent, new RegExp(brand.name));
  assert.equal(document.querySelector('#app-locale select').value, 'en');

  // Every translated node must hold a sentence, not the key that names it.
  const untranslated = [...document.querySelectorAll('[data-i18n]')]
    .filter(node => node.textContent === node.dataset.i18n)
    .map(node => node.dataset.i18n);
  assert.deepEqual(untranslated, [], 'these rendered as their own key');
});

test('the fleet falls back to the demo file and renders its rows', async () => {
  // With no API reachable the page must still show something, because that is
  // what a first-run deployment looks like.
  await startDashboard({ brand, locale: 'en', dict });

  assert.ok(requested.some(u => u.includes('api/v1/fleet')), 'the API was not tried first');
  assert.ok(requested.some(u => u.includes('demo-fleet.json')), 'no fallback was attempted');

  const rows = document.querySelectorAll('#fleet-body tr');
  const sites = demoFleet.customers.flatMap(c => c.sites);
  assert.equal(rows.length, sites.length, 'one row per site');
  assert.match(document.querySelector('#fleet-source').textContent, /demo/i);
});

test('the counters agree with the data behind them', async () => {
  await startDashboard({ brand, locale: 'en', dict });

  const cameras = demoFleet.customers.flatMap(c => c.sites).flatMap(s => s.cameras);
  const online = cameras.filter(c => c.status !== 'offline').length;
  const alerts = cameras.filter(c => c.status !== 'healthy').length;

  assert.equal(document.querySelector('#stat-online').textContent, `${online} / ${cameras.length}`);
  assert.equal(document.querySelector('#stat-alerts').textContent, String(alerts));
  assert.equal(
    document.querySelector('#stat-sites').textContent,
    String(demoFleet.customers.flatMap(c => c.sites).length),
  );
});

test('searching narrows the table and clearing it restores every row', async () => {
  const { render } = await startDashboard({ brand, locale: 'en', dict });
  const all = document.querySelectorAll('#fleet-body tr').length;
  const first = demoFleet.customers[0].sites[0];

  render(first.name.slice(0, 4));
  const narrowed = document.querySelectorAll('#fleet-body tr').length;
  assert.ok(narrowed >= 1 && narrowed <= all, `filter produced ${narrowed} of ${all} rows`);

  render('a-string-no-site-contains');
  assert.equal(document.querySelectorAll('#fleet-body tr').length, 0);

  render('');
  assert.equal(document.querySelectorAll('#fleet-body tr').length, all);
});

test('a camera name cannot inject markup into the table', async () => {
  // Names come from ONVIF, which is to say from whatever the camera reports.
  const { escapeHtml } = await startDashboard({ brand, locale: 'en', dict });
  const escaped = escapeHtml('<img src=x onerror="alert(1)">');
  assert.ok(!escaped.includes('<img'), `markup survived escaping: ${escaped}`);
  assert.ok(!escaped.includes('"'), 'a bare quote can break out of an attribute');

  const dom = new JSDOM(`<div>${escaped}</div>`);
  assert.equal(dom.window.document.querySelector('img'), null, 'the payload became an element');
});

test('escapeHtml handles the values that are not strings', async () => {
  const { escapeHtml } = await startDashboard({ brand, locale: 'en', dict });
  assert.equal(escapeHtml(null), '');
  assert.equal(escapeHtml(undefined), '');
  assert.equal(escapeHtml(42), '42');
});

test('site status is the worst of its cameras, not the first', async () => {
  const { siteStatus } = await startDashboard({ brand, locale: 'en', dict });
  assert.equal(siteStatus([{ status: 'healthy' }, { status: 'healthy' }]), 'healthy');
  assert.equal(siteStatus([{ status: 'healthy' }, { status: 'warning' }]), 'warning');
  assert.equal(
    siteStatus([{ status: 'healthy' }, { status: 'warning' }, { status: 'offline' }]),
    'offline',
    'an offline camera must not be hidden behind healthy ones',
  );
});

test('fmt shows a dash for absent values rather than null', async () => {
  const { fmt } = await startDashboard({ brand, locale: 'en', dict });
  assert.equal(fmt(null), '—');
  assert.equal(fmt(undefined), '—');
  assert.equal(fmt(0), '0', 'zero is a value, not an absence');
  assert.match(fmt(12, ' kbps'), /12 kbps/);
});

// A poll loop that never runs, or runs when it should not, fails silently. The
// page looks fine and the numbers are simply from earlier.

/** A one-camera fleet, so a test can change a status and watch the stats move. */
function fleetWith(status, bitrate) {
  return {
    generated_at: new Date().toISOString(),
    source: 'live',
    customers: [{
      id: 'c1', name: 'Customer', sites: [{
        id: 's1', customer_id: 'c1', name: 'Site', city: 'Barcelona',
        cameras: [{ id: 'cam1', name: 'Cam', site_id: 's1', status, bitrate_kbps: bitrate, last_seen: new Date().toISOString() }],
      }],
    }],
  };
}

test('a refresh repaints the stats from the new fleet', async () => {
  stubFetch({ 'api/v1/fleet': fleetWith('healthy', 2000) });
  const app = await startDashboard({ brand, locale: 'en', dict });
  assert.equal(document.querySelector('#stat-online').textContent, '1 / 1');
  assert.equal(document.querySelector('#stat-throughput').textContent, '2.0 Mbps');

  stubFetch({ 'api/v1/fleet': fleetWith('offline', 0) });
  await app.refresh();

  assert.equal(document.querySelector('#stat-online').textContent, '0 / 1');
  assert.equal(document.querySelector('#stat-alerts').textContent, '1');
  assert.equal(document.querySelector('#stat-throughput').textContent, '0 kbps');
  app.stop();
});

test('a refresh does not repaint under an open modal', async () => {
  // Repainting the fleet while someone reads a camera's telemetry, or fills in
  // the enrollment form, is worse than a number a few seconds old.
  stubFetch({ 'api/v1/fleet': fleetWith('healthy', 2000) });
  const app = await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('.modal-backdrop').classList.add('open');

  stubFetch({ 'api/v1/fleet': fleetWith('offline', 0) });
  await app.refresh();

  assert.equal(document.querySelector('#stat-online').textContent, '1 / 1', 'repainted under a modal');
  app.stop();
});

test('a failed poll leaves the last good numbers alone', async () => {
  // One timed-out request should not look like an outage.
  stubFetch({ 'api/v1/fleet': fleetWith('healthy', 2000) });
  const app = await startDashboard({ brand, locale: 'en', dict });

  globalThis.fetch = async () => { throw new Error('network'); };
  await app.refresh();

  assert.equal(document.querySelector('#stat-online').textContent, '1 / 1');
  assert.equal(document.querySelector('#stat-throughput').textContent, '2.0 Mbps');
  app.stop();
});
