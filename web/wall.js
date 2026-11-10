// The wall: every camera on one screen.
//
// The dashboard shows one camera at a time, in a modal, which is fine for
// asking "is this one working" and useless for watching a premises. This is
// the other view: a tile per camera, in an order the operator chose, with the
// live path behind each one.
//
// Two things shape it, and neither is a style choice:
//
//   - A tile's <video> is never re-created. The page refreshes every five
//     seconds, and a wall that re-rendered its grid on each tick would
//     restart every stream twice a minute. Tiles are built once per camera and
//     patched in place afterwards.
//   - Streams start under a cap, one at a time, with a gap between them. The
//     NVR this was built against has no headroom: three extra sources probed
//     alongside three existing ones took every camera offline for ten minutes
//     (docs/LIVE.md). "Show me all sixteen at once" is that experiment, and a
//     wall that runs it on a press looks like the wall broke the site.
import { t } from './theme.js';

/** How long to wait between starting one tile and the next. */
const START_STAGGER_MS = 1200;
/** How many tiles stream at once unless somebody says otherwise. */
const DEFAULT_CAP = 4;
/** How long a tile's session runs for, which is the longest the gateway grants.
 *  The modal's five minutes suit "is this camera alive"; a wall is left up, and
 *  a session that expires turns the wall black with nothing on screen saying why. */
const SESSION_SECONDS = 3600;

const LAYOUT_KEY = 'wallLayout';
const ORDER_KEY = 'wallOrder';
const CAP_KEY = 'wallCap';

/** localStorage, but never a reason for the page not to load. */
function stored(key, fallback) {
  try { return localStorage.getItem(key) ?? fallback; } catch { return fallback; }
}
function store(key, value) {
  try { localStorage.setItem(key, value); } catch { /* private window, or no storage at all */ }
}

export function createWall(ctx) {
  const { dict, cameras, openLivePeer, isLive, storagePluginId, recordNow, saveClip } = ctx;
  const view = document.querySelector('#wall-view');
  const grid = document.querySelector('#wall-grid');
  const note = document.querySelector('#wall-note');
  const layoutSelect = document.querySelector('#wall-layout');
  const capSelect = document.querySelector('#wall-cap');
  const recordAllButton = document.querySelector('#wall-record-all');
  const clipAllButton = document.querySelector('#wall-clip-all');

  /** Per-camera tile state, keyed by camera id. The nodes outlive a refresh. */
  const live = new Map();
  /** Cameras asked for but waiting for a slot, as camera ids, in order. */
  const queue = [];
  let draining = false;
  /** Which tile is being dragged. The id is kept here rather than read back out
   * of dataTransfer, which is empty in some browsers until the drop fires. */
  let dragging = null;

  let layout = stored(LAYOUT_KEY, 'auto');
  let cap = Number(stored(CAP_KEY, String(DEFAULT_CAP)));
  if (!Number.isFinite(cap) || cap < 0) cap = DEFAULT_CAP;
  layoutSelect.value = layout;
  capSelect.value = String(cap);

  /** The operator's own order, as camera ids. Cameras not in it go last. */
  function savedOrder() {
    try { return JSON.parse(stored(ORDER_KEY, '[]')) || []; } catch { return []; }
  }
  function saveOrder(ids) { store(ORDER_KEY, JSON.stringify(ids)); }

  /** The cameras to show, in the order the wall shows them. */
  function ordered() {
    const all = cameras();
    const position = new Map(savedOrder().map((id, index) => [id, index]));
    return [...all].sort((left, right) => {
      const a = position.has(left.id) ? position.get(left.id) : Number.MAX_SAFE_INTEGER;
      const b = position.has(right.id) ? position.get(right.id) : Number.MAX_SAFE_INTEGER;
      return a - b;
    });
  }

  function swap(from, to) {
    if (!from || !to || from === to) return;
    const ids = ordered().map(camera => camera.id);
    const a = ids.indexOf(from);
    const b = ids.indexOf(to);
    if (a < 0 || b < 0) return;
    [ids[a], ids[b]] = [ids[b], ids[a]];
    saveOrder(ids);
    render();
  }

  function tileFor(camera) {
    const existing = live.get(camera.id);
    if (existing) return existing;
    const node = document.createElement('article');
    node.className = 'wall-tile';
    node.draggable = true;
    node.dataset.wallTile = camera.id;
    node.innerHTML = `
      <video class="wall-video" autoplay muted playsinline hidden></video>
      <div class="wall-placeholder" data-wall-status></div>
      <div class="wall-tile-bar">
        <span class="wall-tile-name"></span>
        <span class="health-pill"></span>
        <button class="button small primary" data-wall-watch></button>
        <button class="button small" data-wall-stop hidden></button>
      </div>`;
    const tile = {
      id: camera.id,
      node,
      video: node.querySelector('.wall-video'),
      status: node.querySelector('[data-wall-status]'),
      name: node.querySelector('.wall-tile-name'),
      pill: node.querySelector('.health-pill'),
      watch: node.querySelector('[data-wall-watch]'),
      stop: node.querySelector('[data-wall-stop]'),
      peer: null,
    };
    tile.watch.textContent = t(dict, 'app.live.start');
    tile.stop.textContent = t(dict, 'app.wall.stop');

    tile.watch.addEventListener('click', () => watch(tile));
    tile.stop.addEventListener('click', () => {
      // An operator stopping one tile has not changed their mind about the
      // others, so the queue keeps its place and takes this slot.
      dropFromQueue(tile.id);
      stop(tile);
      void drain({ stagger: false });
    });

    // Dragging one tile onto another exchanges them, rather than inserting
    // before it: an operator arranging a wall is pairing a position with a
    // camera, and a swap leaves every other position where it was.
    node.addEventListener('dragstart', event => {
      dragging = camera.id;
      event.dataTransfer?.setData?.('text/plain', camera.id);
      node.classList.add('wall-dragging');
    });
    node.addEventListener('dragend', () => {
      dragging = null;
      node.classList.remove('wall-dragging');
    });
    // One tile, the size of the screen. The wall is for noticing; this is for
    // looking, and it is the gesture every other wall in this industry uses.
    node.addEventListener('dblclick', () => { void node.requestFullscreen?.(); });
    node.addEventListener('dragover', event => event.preventDefault());
    node.addEventListener('drop', event => {
      event.preventDefault();
      swap(dragging, camera.id);
      dragging = null;
      node.classList.remove('wall-dragging');
    });

    live.set(camera.id, tile);
    return tile;
  }

  /** Text and pills only: anything that touches <video> would restart a stream. */
  function paint(tile, camera) {
    tile.name.textContent = camera.name || camera.id;
    tile.node.title = camera.site_name ? `${camera.name} — ${camera.site_name}` : camera.name || camera.id;
    const status = camera.status || 'warning';
    tile.pill.className = `health-pill ${status}`;
    tile.pill.textContent = t(dict, `app.${status}`);
    // Buttons only when nothing is in flight: a refresh landing mid-negotiation
    // would otherwise offer Live again for a camera that is already starting.
    if (!tile.peer && !tile.starting) {
      tile.watch.hidden = false;
      tile.stop.hidden = true;
    }
  }

  function render() {
    const shown = ordered();
    const wanted = new Set(shown.map(camera => camera.id));
    // A camera that left the fleet takes its stream with it.
    for (const [id, tile] of live) {
      if (wanted.has(id)) continue;
      stop(tile);
      tile.node.remove();
      live.delete(id);
    }
    for (const camera of shown) {
      const tile = tileFor(camera);
      paint(tile, camera);
      // appendChild on a node already in the grid moves it, which is how the
      // order is applied without rebuilding anything.
      grid.appendChild(tile.node);
    }
    applyLayout(shown.length);
    paintNote();
  }

  function applyLayout(count) {
    const columns = layout === 'auto'
      ? Math.max(1, Math.min(4, Math.ceil(Math.sqrt(count || 1))))
      : Number(layout);
    grid.style.setProperty('--wall-columns', String(columns));
  }

  function watching() {
    return [...live.values()].filter(tile => tile.peer).length;
  }
  /** A tile part-way through negotiation holds its slot: the point of the cap
   *  is how many streams the device is being asked for, not how many arrived. */
  function inUse() {
    return [...live.values()].filter(tile => tile.peer || tile.starting).length;
  }
  function room() { return cap === 0 || inUse() < cap; }

  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

  function dropFromQueue(id) {
    const at = queue.indexOf(id);
    if (at >= 0) queue.splice(at, 1);
  }

  /** One camera, because somebody pressed its own button. */
  function watch(tile) {
    if (tile.peer || tile.starting) return;
    if (!isLive()) {
      tile.status.textContent = t(dict, 'app.archive.liveRequired');
      return;
    }
    if (!room()) {
      // Not queued: a direct press is about this camera now, and silently
      // putting it in a queue behind three others reads as a dead button.
      tile.status.textContent = t(dict, 'app.wall.capReached');
      return;
    }
    void start(tile);
  }

  async function start(tile) {
    tile.starting = true;
    tile.watch.hidden = true;
    tile.stop.hidden = false;
    paintNote();
    try {
      tile.peer = await openLivePeer(tile.id, {
        sessionSeconds: SESSION_SECONDS,
        onStatus: text => { tile.status.textContent = text; },
        onPeer: peer => { tile.peer = peer; },
        onTrack: stream => {
          tile.video.srcObject = stream;
          tile.video.hidden = false;
          tile.status.textContent = '';
        },
        onStateChange: state => {
          if (!['failed', 'disconnected', 'closed'].includes(state)) return;
          // The slot is gone either way; give it to whoever is waiting. The
          // message goes on after stop(), which clears whatever was there.
          stop(tile);
          tile.status.textContent = t(dict, state === 'failed' ? 'app.live.failed' : 'app.live.disconnected');
          void drain({ stagger: false });
        },
      });
    } catch (error) {
      tile.peer = null;
      tile.video.hidden = true;
      tile.watch.hidden = false;
      tile.stop.hidden = true;
      // Named per tile rather than once for the wall: on a wall of sixteen,
      // "live failed" with no camera attached to it is not a fault report.
      tile.status.textContent = `${t(dict, 'app.live.failed')}: ${error.message}`;
    } finally {
      tile.starting = false;
      paintNote();
      if (!tile.peer) void drain({ stagger: false });
    }
  }

  /** Every camera on the wall, one at a time, within the cap. */
  async function watchAll() {
    if (!isLive()) {
      for (const tile of live.values()) {
        if (!tile.peer) tile.status.textContent = t(dict, 'app.archive.liveRequired');
      }
      return;
    }
    for (const camera of ordered()) {
      const tile = live.get(camera.id);
      if (!tile || tile.peer || tile.starting || queue.includes(tile.id)) continue;
      queue.push(tile.id);
      tile.status.textContent = t(dict, 'app.wall.queued');
    }
    await drain();
  }

  /** Fill the free slots from the queue, with a gap between dials.
   *
   *  The gap is the whole point. Starting a stream means the gateway dialling
   *  the camera, and a recorder that has been measured as having no spare
   *  capacity drops every session when several arrive together. A slot that
   *  frees later is filled at once: replacing one stream is not a ramp. */
  async function drain({ stagger = true } = {}) {
    if (draining) return;
    draining = true;
    try {
      let first = true;
      while (queue.length && room()) {
        if (!first && stagger) await sleep(START_STAGGER_MS);
        first = false;
        const tile = live.get(queue.shift());
        if (!tile || tile.peer || tile.starting) continue;
        tile.status.textContent = '';
        void start(tile);
      }
    } finally {
      draining = false;
      paintNote();
    }
  }

  function paintNote() {
    const total = live.size;
    const parts = [t(dict, 'app.wall.watching', 'app.wall.watching', { count: watching(), total })];
    if (cap > 0) parts.push(t(dict, 'app.wall.capNote', 'app.wall.capNote', { cap }));
    // Recording the whole wall needs somewhere to put it. Saying so on the bar
    // once beats four tiles each reporting the same missing plugin.
    const storage = storagePluginId();
    const canRecord = Boolean(storage) && isLive();
    recordAllButton.disabled = !canRecord;
    clipAllButton.disabled = !canRecord;
    if (!isLive()) parts.push(t(dict, 'app.archive.liveRequired'));
    else if (!storage) parts.push(t(dict, 'app.archive.storageMissing'));
    note.textContent = parts.join(' · ');
  }

  /** The same command over every camera on the wall, each reporting in its own
   *  tile. One camera failing is one tile saying so, not a wall-wide error. */
  async function fanOut(send, button) {
    if (button.disabled) return;
    button.disabled = true;
    try {
      await Promise.all([...live.values()].map(async tile => {
        try {
          await send(tile.id, text => { tile.status.textContent = text; });
        } catch (error) {
          tile.status.textContent = `${t(dict, 'app.archive.failed')}: ${error.message}`;
        }
      }));
    } finally {
      paintNote();
    }
  }

  function stop(tile) {
    if (tile.peer) {
      try { tile.peer.close(); } catch { /* already gone */ }
      tile.peer = null;
    }
    tile.video.srcObject = null;
    tile.video.hidden = true;
    tile.watch.hidden = false;
    tile.stop.hidden = true;
    tile.status.textContent = '';
  }

  function open() {
    view.hidden = false;
    document.querySelector('.dashboard').hidden = true;
    render();
  }

  function close() {
    stopAll();
    view.hidden = true;
    document.querySelector('.dashboard').hidden = false;
  }

  function stopAll() {
    queue.length = 0;
    for (const tile of live.values()) stop(tile);
    paintNote();
  }

  function isOpen() { return !view.hidden; }

  layoutSelect.addEventListener('change', () => {
    layout = layoutSelect.value;
    store(LAYOUT_KEY, layout);
    applyLayout(live.size);
  });
  capSelect.addEventListener('change', () => {
    cap = Number(capSelect.value);
    store(CAP_KEY, String(cap));
    paintNote();
  });
  document.querySelector('#wall-close').addEventListener('click', close);
  document.querySelector('#wall-stop-all').addEventListener('click', stopAll);
  document.querySelector('#wall-watch-all').addEventListener('click', () => void watchAll());
  recordAllButton.addEventListener('click', () => void fanOut(recordNow, recordAllButton));
  clipAllButton.addEventListener('click', () => void fanOut(saveClip, clipAllButton));

  return {
    open, close, stopAll, watchAll, isOpen,
    recordAll: () => fanOut(recordNow, recordAllButton),
    clipAll: () => fanOut(saveClip, clipAllButton),
    /** Called by the dashboard's poll: new numbers, same streams. */
    repaint: () => { if (isOpen()) render(); },
  };
}
