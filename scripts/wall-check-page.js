// The wall's half of `make check-live`, injected into a copy of the real
// dashboard rather than standing in for it. @PASSWORD@ is replaced with the
// run's generated password, and this file is served out of a temporary
// directory — it logs in, so it has no business in web/.
//
// It is loaded as a module before dashboard.js, which is the only place two
// things can happen in time: the session has to exist before requireSession
// asks for one, and RTCPeerConnection has to be recorded before the wall opens
// any, because getStats() is the only way to tell pixels from a connection.
//
// What this can prove and no unit test can: that the grid has real boxes with
// real sizes, and that a tile ends up with decoded frames in it. jsdom has no
// layout engine and no media stack, so a wall whose tiles collapsed to nothing
// would pass every test in web/tests.
const say = message => { try { dump('CHECKWALL ' + message + '\n'); } catch (error) { /* not Firefox */ } };
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

const peers = [];
const Native = window.RTCPeerConnection;
window.RTCPeerConnection = function Recorded(config) {
  const peer = new Native(config);
  peers.push(peer);
  return peer;
};

async function until(what, seconds = 40) {
  for (let attempt = 0; attempt < seconds * 10; attempt += 1) {
    const found = what();
    if (found) return found;
    await sleep(100);
  }
  throw new Error('timed out waiting for the page');
}

const login = await fetch('/api/v1/auth/login', {
  method: 'POST', headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ email: 'admin@localhost', password: '@PASSWORD@' }),
});
say(`login=${login.status}`);

// Not awaited: dashboard.js is the next module on the page and cannot run
// until this one finishes.
(async () => {
  try {
    // The link is in the static markup; its handler is not, and arrives when
    // the dashboard has finished its first render. So wait for the fleet to be
    // on the page, and keep pressing until the wall answers.
    await until(() => document.querySelector('#fleet-body tr'));
    const link = document.querySelector('#wall-link');
    const tiles = await until(() => {
      link.click();
      const found = document.querySelectorAll('[data-wall-tile]');
      return found.length ? [...found] : null;
    });
    say(`tiles=${tiles.length}`);

    // The layout, through the browser's own box model.
    const grid = document.querySelector('#wall-grid');
    const layout = document.querySelector('#wall-layout');
    layout.value = '2';
    layout.dispatchEvent(new Event('change', { bubbles: true }));
    await sleep(200);
    say(`columns=${getComputedStyle(grid).gridTemplateColumns}`);
    const box = tiles[0].getBoundingClientRect();
    say(`tile=${Math.round(box.width)}x${Math.round(box.height)}`);

    document.querySelector('#wall-watch-all').click();
    let decoded = 0;
    let size = '0x0';
    for (let second = 0; second < 60 && decoded === 0; second += 1) {
      await sleep(1000);
      for (const peer of peers) {
        (await peer.getStats()).forEach(report => {
          if (report.type !== 'inbound-rtp' || report.kind !== 'video') return;
          decoded = Math.max(decoded, report.framesDecoded || 0);
          if (report.frameWidth) size = `${report.frameWidth}x${report.frameHeight}`;
        });
      }
      if (second % 10 === 0) say(`waiting=${second}s peers=${peers.length} decoded=${decoded}`);
    }
    const video = tiles[0].querySelector('.wall-video');
    say(`video=${video.videoWidth}x${video.videoHeight} hidden=${video.hidden}`);
    say(`peers=${peers.length} decoded=${decoded} size=${size}`);
    say(`note=${(document.querySelector('#wall-note').textContent || '').slice(0, 140)}`);
  } catch (error) {
    say(`ERROR ${error}`);
  }
  say('done');
  window.close();
})();
