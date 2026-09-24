// The login gate. The dashboard must not paint before a session exists, and
// any later 401 must flip back to the login view.
import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');
const dict = JSON.parse(read('locales/en.json'));

let requireSession, installUnauthorizedTrap;
let responses; // path suffix -> status
let bodies; // path suffix -> the body that was posted to it

function stubFetch() {
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if (options.body) {
      const key = Object.keys(responses).find(suffix => path.includes(suffix)) ?? path;
      bodies[key] = options.body;
    }
    const match = Object.entries(responses).find(([suffix]) => path.includes(suffix));
    const status = match ? match[1] : 404;
    return {
      ok: status >= 200 && status < 300,
      status,
      json: async () => ({}),
      _options: options,
    };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'Event', 'FormData']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key];
  }
  return dom;
}

beforeEach(async () => {
  loadPage();
  responses = {};
  bodies = {};
  stubFetch();
  ({ requireSession, installUnauthorizedTrap } = await import('../auth.js'));
});

test('an alive session passes straight through without showing the login view', async () => {
  responses['api/v1/auth/session'] = 204;
  await requireSession(dict);
  assert.ok(!document.querySelector('#login-view').classList.contains('open'));
});

test('no session shows the login view and a successful login resolves the gate', async () => {
  responses['api/v1/auth/session'] = 401;
  responses['api/v1/auth/login'] = 204;
  const gate = requireSession(dict);

  // The view is up and translated.
  const view = document.querySelector('#login-view');
  assert.ok(view.classList.contains('open'), 'the login view did not open');
  const heading = view.querySelector('[data-i18n="app.auth.title"]');
  assert.notEqual(heading.textContent, '', 'the login view is untranslated');

  const form = document.querySelector('#login-form');
  form.querySelector('input[name=email]').value = 'owner@example.test';
  form.querySelector('input[name=password]').value = 'a fine password';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await gate;
  assert.ok(!view.classList.contains('open'), 'the login view stayed up after login');
  // Who is logging in goes with the password: there is no one shared
  // account any more.
  const sent = JSON.parse(bodies['api/v1/auth/login']);
  assert.equal(sent.email, 'owner@example.test');
  assert.equal(sent.password, 'a fine password');
});

test('a wrong password shows the error line and keeps the view open', async () => {
  responses['api/v1/auth/session'] = 401;
  responses['api/v1/auth/login'] = 401;
  const gate = requireSession(dict);
  const form = document.querySelector('#login-form');
  form.querySelector('input[name=password]').value = 'wrong';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(document.querySelector('#login-error').hidden, false);
  assert.ok(document.querySelector('#login-view').classList.contains('open'));
  // The gate must still be pending; a resolved gate would paint the dashboard.
  let settled = false;
  gate.then(() => { settled = true; });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(settled, false, 'the gate resolved on a failed login');
});

test('a 401 from any API call flips back to the login view', async () => {
  responses['api/v1/auth/session'] = 204;
  await requireSession(dict);
  installUnauthorizedTrap();
  responses['api/v1/fleet'] = 401;
  await globalThis.fetch('api/v1/fleet');
  assert.ok(document.querySelector('#login-view').classList.contains('open'),
    'an expired session did not bring the login view back');
});

test('the account modal changes the password and reports success', async () => {
  const { wireAccountModal } = await import('../auth.js');
  responses['api/v1/auth/password'] = 204;
  wireAccountModal(dict);

  document.querySelector('.avatar').dispatchEvent(new Event('click', { bubbles: true }));
  assert.ok(document.querySelector('#account-modal').classList.contains('open'));

  const form = document.querySelector('#password-form');
  form.querySelector('input[name=current]').value = 'the old password';
  form.querySelector('input[name=new]').value = 'a long enough new one';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(document.querySelector('#password-done').hidden, false);
});

test('a rejected password change shows the error, not the success line', async () => {
  const { wireAccountModal } = await import('../auth.js');
  responses['api/v1/auth/password'] = 403;
  wireAccountModal(dict);
  const form = document.querySelector('#password-form');
  form.querySelector('input[name=current]').value = 'wrong';
  form.querySelector('input[name=new]').value = 'a long enough new one';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(document.querySelector('#password-error').hidden, false);
  assert.equal(document.querySelector('#password-done').hidden, true);
});
