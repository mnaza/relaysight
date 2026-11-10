// Pressing Live, with a stand-in for the browser's own WebRTC.
//
// The live path had no test at all until a real browser met it. Two things it
// found: a browser that cannot receive H.264 was answered with WebRTC's own
// wording, and a browser that gathers no ICE candidates left the page loading
// forever while the gateway waited twenty seconds and gave up. Both are about
// what the person in front of the screen is told.
import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');
const brand = JSON.parse(read('brand.json'));
const dict = JSON.parse(read('locales/en.json'));

const SITE = {
  generated_at: new Date().toISOString(),
  source: 'live',
  customers: [{
    id: 'cust-1', name: 'Customer',
    sites: [{
      id: 'site-1', customer_id: 'cust-1', name: 'Site', city: 'Town',
      cameras: [{ id: 'cam-1', name: 'Front door', site_id: 'site-1', status: 'healthy', fps: 25, bitrate_kbps: 2000, last_seen: new Date().toISOString() }],
    }],
  }],
};

let startDashboard;
let posted;

/** A peer connection that gathers what the test tells it to. */
function fakePeer({ candidates }) {
  return class {
    constructor() {
      this.iceGatheringState = 'complete';
      this.connectionState = 'new';
      this.localDescription = null;
      this.listeners = {};
    }
    addTransceiver() {}
    addEventListener(name, handler) { this.listeners[name] = handler; }
    removeEventListener() {}
    async createOffer() { return { type: 'offer', sdp: 'v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n' }; }
    async setLocalDescription(offer) {
      const lines = candidates.map(c => `a=candidate:${c}\r\n`).join('');
      this.localDescription = { type: offer.type, sdp: offer.sdp + lines };
    }
    async setRemoteDescription() {}
    close() {}
  };
}

function stubFetch() {
  posted = [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if (options.method === 'POST') posted.push(path);
    const body =
      path.includes('api/v1/fleet') ? SITE
      : path.includes('api/v1/rtc/config') ? { ice_servers: [] }
      : path.includes('api/v1/cameras/telemetry') ? []
      : path.includes('api/v1/system/edition') ? { edition: 'community', plan: 'community-self-hosted', camera_limit: null, capabilities: [] }
      : path.includes('api/v1/cameras/cam-1/live') ? { command_id: 'cmd-1' }
      : path.includes('api/v1/commands/cmd-1') ? { status: 'succeeded', result: { live: { sdp: 'v=0\r\n', sdp_type: 'answer' } } }
      : [];
    return { ok: true, status: 200, json: async () => body };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'localStorage', 'URL', 'Event', 'FormData', 'MediaStream']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key] ?? class {};
  }
}

async function pressLive() {
  await startDashboard({ brand, locale: 'en', dict });
  // The camera cards live behind a site's row: the fleet table opens the
  // telemetry modal, and the Live button is on the card in there.
  const site = document.querySelector('#fleet-body [data-live]');
  assert.ok(site, 'no site row was rendered');
  site.dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 5));
  const button = document.querySelector('[data-live-camera]');
  assert.ok(button, 'no Live button was rendered');
  assert.equal(button.disabled, false, 'the Live button is disabled for a live fleet');
  button.dispatchEvent(new Event('click', { bubbles: true }));
  for (let tick = 0; tick < 40; tick += 1) await new Promise(resolve => setTimeout(resolve, 5));
  return document.querySelector('#live-player-status');
}

beforeEach(async () => {
  loadPage();
  stubFetch();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('a browser that gathered no network path is told so, not left loading', async () => {
  globalThis.RTCPeerConnection = fakePeer({ candidates: [] });
  const status = await pressLive();
  assert.match(status.textContent, /no network path|no ICE candidates/i,
    `the page said "${status.textContent}" instead of naming the missing path`);
  assert.ok(!posted.some(path => path.includes('/live')),
    'an offer with no candidates was sent to the gateway anyway');
});

test('a browser that gathered a path gets as far as the gateway', async () => {
  globalThis.RTCPeerConnection = fakePeer({
    candidates: ['1 1 UDP 2122252543 192.168.1.10 55146 typ host'],
  });
  await pressLive();
  assert.ok(posted.some(path => path.includes('api/v1/cameras/cam-1/live')),
    `the offer never reached the gateway; posted: ${posted.join(', ')}`);
});
