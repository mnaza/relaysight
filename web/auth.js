// The session gate. Runs before the dashboard paints, owns the login view,
// and traps any later 401 so an expired session brings the login back
// instead of quietly demoting the page to demo data.
import { t } from './theme.js';

const view = () => document.querySelector('#login-view');

function showLogin(dict) {
  const node = view();
  node.querySelectorAll('[data-i18n]').forEach(el => el.textContent = t(dict, el.dataset.i18n, el.dataset.i18n));
  node.classList.add('open');
}

/**
 * Resolves once a session exists — immediately, or after a successful login.
 *
 * The login view and its submit handler are wired up before the session
 * check is even sent, not after it comes back: a caller that never awaits
 * this promise (it only cares that the gate eventually settles) must still
 * find the view open and the form live the instant this function returns.
 * An alive session closes the view again once the check resolves.
 */
export async function requireSession(dict) {
  showLogin(dict);
  let resolveGate;
  const gate = new Promise(resolve => { resolveGate = resolve; });
  const form = document.querySelector('#login-form');
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const password = new FormData(form).get('password');
    const response = await fetch('api/v1/auth/login', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ password }),
    }).catch(() => ({ status: 0 }));
    if (response.status === 204) {
      view().classList.remove('open');
      document.querySelector('#login-error').hidden = true;
      resolveGate();
    } else {
      document.querySelector('#login-error').hidden = false;
    }
  });

  const alive = await fetch('api/v1/auth/session').then(r => r.status === 204).catch(() => false);
  if (alive) {
    view().classList.remove('open');
    resolveGate();
  }
  return gate;
}

/** After this, any 401 from the API re-opens the login view. */
export function installUnauthorizedTrap() {
  const original = globalThis.fetch;
  globalThis.fetch = async (url, options) => {
    const response = await original(url, options);
    const path = String(url);
    if (response.status === 401 && path.includes('api/') && !path.includes('api/v1/auth/')) {
      view().classList.add('open');
    }
    return response;
  };
}

export async function logout() {
  await fetch('api/v1/auth/logout', { method: 'POST' }).catch(() => {});
  location.reload();
}

/** Returns the response status; 204 means changed. */
export async function changePassword(current, next) {
  const response = await fetch('api/v1/auth/password', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ current, new: next }),
  }).catch(() => ({ status: 0 }));
  return response.status;
}

/** Avatar opens the account modal; the form changes the password; the button signs out. */
export function wireAccountModal(dict) {
  const modal = document.querySelector('#account-modal');
  modal.querySelectorAll('[data-i18n]').forEach(el => el.textContent = t(dict, el.dataset.i18n, el.dataset.i18n));
  document.querySelector('.avatar').addEventListener('click', () => modal.classList.add('open'));
  modal.querySelectorAll('[data-close-account]').forEach(node =>
    node.addEventListener('click', () => modal.classList.remove('open')));
  document.querySelector('#logout-button').addEventListener('click', () => logout());
  const form = document.querySelector('#password-form');
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const data = new FormData(form);
    const status = await changePassword(data.get('current'), data.get('new'));
    document.querySelector('#password-done').hidden = status !== 204;
    document.querySelector('#password-error').hidden = status === 204;
    if (status === 204) form.reset();
  });
}
