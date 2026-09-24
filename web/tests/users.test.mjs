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
