// People, rather than one shared password: the owner's view of who can get in.
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

const users = [
  { id: 'u-1', email: 'owner@example.test', role: 'owner', customer_id: null,
    created_at: new Date().toISOString(), disabled_at: null },
  { id: 'u-2', email: 'tech@example.test', role: 'technician', customer_id: null,
    created_at: new Date().toISOString(), disabled_at: null },
  { id: 'u-3', email: 'gone@example.test', role: 'viewer', customer_id: 'cust-1',
    created_at: new Date().toISOString(), disabled_at: new Date().toISOString() },
];

let startDashboard, posted, userStatus;

function stubFetch({ usersStatus = 200 } = {}) {
  posted = [];
  userStatus = usersStatus;
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if ((options.method || 'GET') === 'POST') {
      posted.push({ path, body: options.body ? JSON.parse(options.body) : null });
      return { ok: true, status: 201, json: async () => ({}) };
    }
    if (path.startsWith('api/v1/users')) {
      return { ok: userStatus === 200, status: userStatus, json: async () => users };
    }
    const body =
      path.endsWith('api/v1/fleet') ? demoFleet
      : path.startsWith('api/v1/health') || path.includes('/health') ? { days: 7, hours: [] }
      : path.includes('api/v1/cameras') || path.includes('api/v1/gateways')
        || path.includes('api/v1/plugins') || path.includes('api/v1/sources')
        || path.includes('api/v1/incidents') || path.includes('api/v1/events') ? []
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

test('an owner sees who can get in, and what each of them may do', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const text = document.querySelector('#users-list').textContent;
  assert.match(text, /owner@example\.test/);
  assert.match(text, /tech@example\.test/);
  // A turned-off account is shown as turned off rather than quietly missing.
  assert.match(text, new RegExp(dict['app.users.disabled']));
  // And a scoped one says which customer it is scoped to.
  assert.match(text, /cust-1/);
});

test('adding a user posts the email, role and scope', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const form = document.querySelector('#user-form');
  form.querySelector('[name="email"]').value = 'new@example.test';
  form.querySelector('[name="password"]').value = 'a perfectly good passphrase';
  form.querySelector('[name="role"]').value = 'viewer';
  form.querySelector('[name="customerId"]').value = 'cust-7';
  form.dispatchEvent(new Event('submit', { cancelable: true, bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const sent = posted.find(entry => entry.path === 'api/v1/users');
  assert.ok(sent, JSON.stringify(posted));
  assert.deepEqual(sent.body, {
    email: 'new@example.test',
    password: 'a perfectly good passphrase',
    role: 'viewer',
    customer_id: 'cust-7',
  });
});

test('a viewer is not shown a panel they cannot use', async () => {
  // The API refuses them anyway; showing the panel would only be a promise
  // the control plane then breaks.
  stubFetch({ usersStatus: 403 });
  await startDashboard({ brand, locale: 'en', dict });
  assert.ok(document.querySelector('#users').hidden, 'the users panel stayed up for a viewer');
});

test('a plugin the core is leaving alone says so on its card', async () => {
  // "Offline" and "we stopped trying for two minutes" are different facts,
  // and the second one explains why nothing is being retried.
  const plugins = [{
    endpoint: 'http://sink:9003', placement: 'control_plane', enabled: true,
    reachable: false, cooling_off_seconds: 120, last_error: 'connection refused',
    manifest: { id: 'webhook-sink', name: 'Webhook', version: '0.1.0', protocol_version: 1,
                vendor: 'example', description: '', capabilities: ['event_sink'] },
  }];
  const inner = globalThis.fetch;
  globalThis.fetch = async (url, options) => {
    if (String(url).startsWith('api/v1/plugins')) {
      return { ok: true, status: 200, json: async () => plugins };
    }
    return inner(url, options);
  };
  await startDashboard({ brand, locale: 'en', dict });
  const card = document.querySelector('#plugins-grid .plugin-card');
  assert.match(card.textContent, /120/);
  assert.match(card.textContent, new RegExp(dict['app.plugins.cooling'].split('{')[0].trim()));
});

test('an owner can connect a plugin from the dashboard', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const form = document.querySelector('#plugin-form');
  assert.ok(!form.hidden, 'an owner sees the form');
  form.querySelector('[name="endpoint"]').value = 'http://storage:9002';
  form.querySelector('[name="tokenEnv"]').value = 'STORAGE_PLUGIN_TOKEN';
  form.dispatchEvent(new Event('submit', { cancelable: true, bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 10));

  const sent = posted.find(entry => entry.path === 'api/v1/plugins/registrations');
  assert.ok(sent, JSON.stringify(posted));
  assert.equal(sent.body.endpoint, 'http://storage:9002');
  assert.equal(sent.body.token_env, 'STORAGE_PLUGIN_TOKEN');
  assert.equal(sent.body.customer_id, null);
});

test('a viewer is not offered the plugin form either', async () => {
  stubFetch({ usersStatus: 403 });
  await startDashboard({ brand, locale: 'en', dict });
  assert.ok(document.querySelector('#plugin-form').hidden);
});
