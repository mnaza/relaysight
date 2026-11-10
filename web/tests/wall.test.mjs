// The wall: every camera on one screen.
//
// A dashboard that shows one camera at a time, in a modal, cannot be used to
// watch a premises. These tests are about the two things a wall gets wrong in
// ways nothing else notices: it tears down a stream when the page refreshes,
// and it opens every camera at once on a device that cannot take it.
import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');
const brand = JSON.parse(read('brand.json'));
const dict = JSON.parse(read('locales/en.json'));

const CAMERAS = ['cam-1', 'cam-2', 'cam-3', 'cam-4'];

function fleet(source = 'live') {
  return {
    generated_at: new Date().toISOString(),
    source,
    customers: [{
      id: 'cust-1', name: 'Customer',
      sites: [{
        id: 'site-1', customer_id: 'cust-1', name: 'Site', city: 'Town',
        cameras: CAMERAS.map((id, index) => ({
          id, name: `Camera ${index + 1}`, site_id: 'site-1', status: 'healthy',
          fps: 25, bitrate_kbps: 2000, last_seen: new Date().toISOString(),
        })),
      }],
    }],
  };
}

/** How many peers the page has open right now, which is what the cap is about. */
let openPeers;

/** A peer connection that answers instantly and hands over a track. */
function fakePeer() {
  return class {
    constructor() {
      this.iceGatheringState = 'complete';
      this.connectionState = 'new';
      this.localDescription = null;
      this.listeners = {};
      openPeers.add(this);
    }
    addTransceiver() {}
    addEventListener(name, handler) { this.listeners[name] = handler; }
    removeEventListener() {}
    async createOffer() { return { type: 'offer', sdp: 'v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n' }; }
    async setLocalDescription(offer) {
      this.localDescription = {
        type: offer.type,
        sdp: `${offer.sdp}a=candidate:1 1 UDP 2122252543 192.168.1.10 55146 typ host\r\n`,
      };
    }
    async setRemoteDescription() {
      this.connectionState = 'connected';
      this.listeners.track?.({ track: { kind: 'video' }, streams: [{ id: 'stream' }] });
    }
    close() { openPeers.delete(this); this.connectionState = 'closed'; }
  };
}

let posted;
let bodies;

function stubFetch({ storage = true, slowCameras = new Set() } = {}) {
  posted = [];
  bodies = [];
  const plugins = storage
    ? [{ reachable: true, manifest: { id: 'storage-s3', capabilities: ['storage_blob'] } }]
    : [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if (options.method === 'POST') {
      posted.push(path);
      bodies.push({ path, body: options.body ? JSON.parse(options.body) : null });
    }
    const camera = path.match(/cameras\/([^/]+)\//)?.[1];
    const body =
      path.includes('api/v1/fleet') ? fleet(globalThis.__fleetSource ?? 'live')
      : path.includes('api/v1/rtc/config') ? { ice_servers: [] }
      : path.includes('api/v1/plugins') ? plugins
      : path.includes('api/v1/system/edition') ? { edition: 'community', plan: 'community-self-hosted', camera_limit: null, capabilities: ['storage_plugins'] }
      : path.endsWith('/live') ? { command_id: `live-${camera}` }
      : path.endsWith('/recordings') ? { command_id: `rec-${camera}` }
      : path.endsWith('/clips') ? { command_id: `clip-${camera}` }
      : path.includes('api/v1/commands/live-') ? {
        status: slowCameras.has(path.split('live-')[1]) ? 'running' : 'succeeded',
        result: { live: { sdp: 'v=0\r\n', sdp_type: 'answer' } },
      }
      : path.includes('api/v1/commands/') ? { status: 'succeeded', result: {} }
      : [];
    return { ok: true, status: 200, json: async () => body };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'localStorage', 'URL', 'Event', 'FormData', 'MediaStream', 'DataTransfer']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key] ?? class {};
  }
  globalThis.CustomEvent = dom.window.CustomEvent;
}

let startDashboard;

async function openWall() {
  const handles = await startDashboard({ brand, locale: 'en', dict });
  document.querySelector('#wall-link').dispatchEvent(new Event('click', { bubbles: true }));
  await settle();
  return handles;
}

const settle = async (ticks = 30) => {
  for (let tick = 0; tick < ticks; tick += 1) await new Promise(resolve => setTimeout(resolve, 2));
};

const tiles = () => [...document.querySelectorAll('[data-wall-tile]')];
const order = () => tiles().map(tile => tile.dataset.wallTile);

beforeEach(async () => {
  openPeers = new Set();
  globalThis.__fleetSource = 'live';
  loadPage();
  stubFetch();
  globalThis.RTCPeerConnection = fakePeer();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('the wall has a tile for every camera in the fleet', async () => {
  await openWall();
  assert.equal(document.querySelector('#wall-view').hidden, false, 'the wall never opened');
  assert.deepEqual(order(), CAMERAS);
  assert.equal(
    tiles()[0].querySelector('.wall-tile-name').textContent,
    'Camera 1',
    'a tile does not name its camera',
  );
});

test('the order is the operator\'s, and dragging one tile onto another rewrites it', async () => {
  localStorage.setItem('wallOrder', JSON.stringify(['cam-3', 'cam-1']));
  await openWall();
  assert.deepEqual(order(), ['cam-3', 'cam-1', 'cam-2', 'cam-4'],
    'a saved order was not applied, or cameras it does not mention did not go last');

  const [first, , third] = tiles();
  first.dispatchEvent(new Event('dragstart', { bubbles: true }));
  third.dispatchEvent(new Event('dragover', { bubbles: true }));
  third.dispatchEvent(new Event('drop', { bubbles: true }));
  await settle(4);

  assert.deepEqual(order(), ['cam-2', 'cam-1', 'cam-3', 'cam-4'],
    'the two tiles did not exchange places');
  assert.deepEqual(JSON.parse(localStorage.getItem('wallOrder')), ['cam-2', 'cam-1', 'cam-3', 'cam-4'],
    'the new order was not written down, so a reload loses it');
});

test('watching a tile reaches that camera and puts its video in that tile', async () => {
  await openWall();
  tiles()[0].querySelector('[data-wall-watch]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle();
  assert.ok(posted.some(path => path.includes('api/v1/cameras/cam-1/live')),
    `no offer went to cam-1; posted: ${posted.join(', ')}`);
  assert.equal(openPeers.size, 1, 'a tile opened more than one peer, or none');
  assert.equal(tiles()[0].querySelector('.wall-video').hidden, false, 'the video stayed hidden');
  assert.ok(tiles()[0].querySelector('.wall-video').srcObject, 'no stream was attached to the tile');
  assert.equal(tiles()[1].querySelector('.wall-video').hidden, true, 'another tile started on its own');
});

test('a refresh brings new numbers without restarting the streams', async () => {
  // The dashboard polls every five seconds. A wall that rebuilt its grid on
  // each tick would restart every camera twice a minute, and the symptom —
  // video that stutters back to black — looks like the network, not the page.
  const handles = await openWall();
  tiles()[0].querySelector('[data-wall-watch]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle();
  const video = tiles()[0].querySelector('.wall-video');

  await handles.refresh();
  await settle(5);

  assert.equal(tiles()[0].querySelector('.wall-video'), video, 'the tile\'s video element was replaced');
  assert.ok(video.srcObject, 'the stream was dropped by a refresh');
  assert.equal(openPeers.size, 1, 'the refresh left a stray peer behind');
  assert.equal(posted.filter(path => path.endsWith('/live')).length, 1,
    'the refresh asked the gateway for the stream again');
});

test('watch all fills the slots the cap allows, queues the rest, and says so', async () => {
  // Why there is a cap at all: the recorder this was built against lost every
  // camera for ten minutes when three extra streams were opened alongside the
  // three it already had. A wall that opens sixteen at once is that, on a press.
  const handles = await openWall();
  const capSelect = document.querySelector('#wall-cap');
  capSelect.value = '2';
  capSelect.dispatchEvent(new Event('change', { bubbles: true }));

  await handles.wall.watchAll();
  await settle();

  assert.equal(openPeers.size, 2, `the cap of 2 let ${openPeers.size} streams start`);
  const queued = tiles().slice(2).map(tile => tile.querySelector('[data-wall-status]').textContent);
  for (const text of queued) {
    assert.match(text, /waiting/i, `a camera over the cap says "${text}" rather than that it is waiting`);
  }
  assert.match(document.querySelector('#wall-note').textContent, /2 of 4 watching/i,
    `the wall reports "${document.querySelector('#wall-note').textContent}"`);
});

test('a slot that frees goes to a camera that was waiting', async () => {
  const handles = await openWall();
  const capSelect = document.querySelector('#wall-cap');
  capSelect.value = '2';
  capSelect.dispatchEvent(new Event('change', { bubbles: true }));
  await handles.wall.watchAll();
  await settle();

  tiles()[0].querySelector('[data-wall-stop]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle();

  assert.equal(openPeers.size, 2, 'the freed slot was left dark');
  assert.ok(posted.some(path => path.includes('cam-3/live')),
    `the waiting camera was never started; posted: ${posted.join(', ')}`);
  assert.equal(tiles()[0].querySelector('.wall-video').hidden, true, 'the stopped tile is still showing video');
});

test('record all asks every camera on the wall for a recording', async () => {
  await openWall();
  document.querySelector('#wall-record-all').dispatchEvent(new Event('click', { bubbles: true }));
  await settle(60);
  for (const id of CAMERAS) {
    assert.ok(posted.some(path => path === `api/v1/cameras/${id}/recordings`),
      `${id} was never asked to record; posted: ${posted.join(', ')}`);
  }
  for (const tile of tiles()) {
    assert.match(tile.querySelector('[data-wall-status]').textContent, /ready|saved/i,
      `a tile does not report what happened: "${tile.querySelector('[data-wall-status]').textContent}"`);
  }
});

test('save the last minutes asks every camera for a clip', async () => {
  await openWall();
  document.querySelector('#wall-clip-all').dispatchEvent(new Event('click', { bubbles: true }));
  await settle(60);
  for (const id of CAMERAS) {
    assert.ok(posted.some(path => path === `api/v1/cameras/${id}/clips`),
      `${id} was never asked for a clip; posted: ${posted.join(', ')}`);
  }
});

test('with nowhere to put a recording the wall says so once, not four times', async () => {
  stubFetch({ storage: false });
  await openWall();
  const record = document.querySelector('#wall-record-all');
  assert.equal(record.disabled, true, 'the button offers a recording with no storage plugin behind it');
  assert.match(document.querySelector('#wall-note').textContent, /storage/i,
    `the wall does not say why: "${document.querySelector('#wall-note').textContent}"`);
  record.dispatchEvent(new Event('click', { bubbles: true }));
  await settle(10);
  assert.deepEqual(posted.filter(path => path.includes('/recordings')), [],
    'a disabled button still sent commands');
});

test('a demo fleet shows the wall but refuses to pretend it can stream', async () => {
  globalThis.__fleetSource = 'demo';
  await openWall();
  assert.equal(tiles().length, CAMERAS.length, 'the wall is empty in demo mode');
  tiles()[0].querySelector('[data-wall-watch]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle(10);
  assert.deepEqual(posted.filter(path => path.endsWith('/live')), [],
    'a demo fleet negotiated WebRTC against an API that is not there');
  assert.match(tiles()[0].querySelector('[data-wall-status]').textContent, /live/i,
    `the tile says "${tiles()[0].querySelector('[data-wall-status]').textContent}"`);
});

test('a wall tile asks for the longest session the gateway allows', async () => {
  // The modal's five minutes are fine for "is this camera alive". A wall is
  // left up, and a five-minute session turns into sixteen black tiles with
  // nothing on screen to explain them.
  await openWall();
  tiles()[0].querySelector('[data-wall-watch]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle();
  const offer = bodies.find(entry => entry.path.endsWith('/live'));
  assert.ok(offer, 'no offer was sent');
  assert.equal(offer.body.session_seconds, 3600,
    `a tile asked for ${offer.body.session_seconds}s, which is not the longest the gateway allows`);
});

test('leaving the wall by any other link puts the dashboard back', async () => {
  // The wall hides the dashboard. A sidebar link that scrolls to a section
  // nobody can see is a page that looks broken, and the only way out was the
  // browser's back button.
  const handles = await openWall();
  document.querySelector('a[href="#overview"]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle(5);
  assert.equal(document.querySelector('#wall-view').hidden, true, 'the wall stayed open');
  assert.equal(document.querySelector('.dashboard').hidden, false, 'the dashboard stayed hidden');
  assert.equal(handles.wall.isOpen(), false);
});

test('leaving the wall stops the streams rather than leaving them running', async () => {
  const handles = await openWall();
  tiles()[0].querySelector('[data-wall-watch]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle();
  assert.equal(openPeers.size, 1);
  document.querySelector('#wall-close').dispatchEvent(new Event('click', { bubbles: true }));
  await settle(5);
  assert.equal(openPeers.size, 0, 'a closed wall is still holding sessions open on the gateway');
  assert.equal(handles.wall.isOpen(), false);
});

test('a double click asks for the tile alone, full screen', async () => {
  await openWall();
  const tile = tiles()[0];
  let asked = null;
  tile.requestFullscreen = () => { asked = tile.dataset.wallTile; return Promise.resolve(); };
  tile.dispatchEvent(new Event('dblclick', { bubbles: true }));
  await settle(3);
  assert.equal(asked, 'cam-1', 'a double click did not ask for full screen');
});

test('a session that ends names it on the tile and hands the slot on', async () => {
  // Both halves of this were wrong once and no test noticed: the tile wiped
  // its own message on the way out, and the slot it freed was not offered to
  // the cameras waiting for one.
  const handles = await openWall();
  const capSelect = document.querySelector('#wall-cap');
  capSelect.value = '1';
  capSelect.dispatchEvent(new Event('change', { bubbles: true }));
  tiles()[0].querySelector('[data-wall-watch]').dispatchEvent(new Event('click', { bubbles: true }));
  await settle();
  await handles.wall.watchAll();
  await settle(5);

  const peer = [...openPeers][0];
  peer.connectionState = 'failed';
  peer.listeners.connectionstatechange?.();
  await settle();

  assert.match(tiles()[0].querySelector('[data-wall-status]').textContent, /failed/i,
    `the tile says "${tiles()[0].querySelector('[data-wall-status]').textContent}" about a session that died`);
  assert.ok(posted.some(path => path.includes('cam-2/live')),
    `the freed slot went nowhere; posted: ${posted.join(', ')}`);
});

test('the search box narrows the wall, and the tile says where the camera is', async () => {
  await openWall();
  assert.equal(tiles()[0].title, 'Camera 1 — Site', `a tile's tooltip is "${tiles()[0].title}"`);
  const search = document.querySelector('#fleet-search');
  search.value = 'camera 2';
  search.dispatchEvent(new Event('input', { bubbles: true }));
  await settle(5);
  assert.deepEqual(order(), ['cam-2'], 'the wall ignored the search');
  search.value = '';
  search.dispatchEvent(new Event('input', { bubbles: true }));
  await settle(5);
  assert.deepEqual(order(), CAMERAS, 'clearing the search did not bring the wall back');
});
