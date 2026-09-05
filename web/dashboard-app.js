// The dashboard, as one function of its context.
//
// This file used to be a script: it awaited loadRuntime() at module scope and
// then read `brand`, `locale` and `dict` from module bindings, so importing it
// from a test fetched brand.json and failed, and none of its render functions
// could be called. Everything below is now the body of `startDashboard`, and
// those three are its parameters.
//
// Note the four `innerHTML = \`` blocks: their continuation lines, and the line
// carrying the closing backtick, sit one level out from the code around them.
// That is not an oversight. Everything between the backticks is literal content,
// including the leading whitespace, so indenting those lines changes what the
// browser receives. They were left exactly as they were when this body moved out
// of dashboard.js, and the re-indent of everything else was checked against a
// hash of the rendered page — it comes out identical.

import { brandMark, localeSelect, t } from './theme.js';

export async function startDashboard({ brand, locale, dict }) {
  document.documentElement.lang = locale;
  document.querySelector('#app-brand').appendChild(brandMark(brand));
  document.querySelector('#app-locale').appendChild(localeSelect(brand, locale));
  document.querySelectorAll('[data-i18n]').forEach(node => node.textContent = t(dict, node.dataset.i18n, node.dataset.i18n, { freeCameras: brand.freeTier?.cameras ?? 3 }));
  document.querySelectorAll('[data-i18n-placeholder]').forEach(node => node.placeholder = t(dict, node.dataset.i18nPlaceholder));

  async function tryJson(url, timeoutMs = 1400) {
    const response = await fetch(url, { signal: AbortSignal.timeout(timeoutMs) });
    if (!response.ok) throw new Error(`${response.status}`);
    return response.json();
  }

  async function loadFleet() {
    try { return await tryJson('api/v1/fleet'); }
    catch { return await fetch('demo-fleet.json').then(r => r.json()); }
  }
  async function loadTelemetry() {
    try { return await tryJson('api/v1/cameras'); }
    catch { return []; }
  }
  async function loadEdition() {
    try { return await tryJson('api/v1/system/edition'); }
    catch {
      return {
        edition: 'commercial', plan: 'commercial-free', self_hosted: false, managed: true,
        camera_limit: Number(brand.freeTier?.cameras ?? 3), capabilities: ['plugins','ai_plugins','storage_plugins']
      };
    }
  }
  async function loadPlugins() {
    try { return await tryJson('api/v1/plugins'); }
    catch { return await fetch('demo-plugins.json').then(r => r.json()).catch(() => []); }
  }

  async function loadGateways() {
    try { return await tryJson('api/v1/gateways'); }
    catch { return []; }
  }

  let [fleet, telemetry, edition, plugins, gateways] = await Promise.all([loadFleet(), loadTelemetry(), loadEdition(), loadPlugins(), loadGateways()]);
  let telemetryById = new Map(telemetry.map(camera => [camera.camera_id, camera]));
  const isLive = fleet.source === 'live';
  const sourceTag = document.querySelector('#fleet-source');
  sourceTag.textContent = t(dict, isLive ? 'app.liveData' : 'app.demo');
  sourceTag.classList.toggle('status', isLive);

  let rows = fleet.customers.flatMap(customer => customer.sites.map(site => ({ customer, site })));
  let allCameras = rows.flatMap(row => row.site.cameras);
  // Every number on this screen is computed from what the fleet actually
  // reported. There were five hardcoded ones here -- a 99.72% uptime and four
  // invented trends like "+1 this month" -- sitting beside three real ones,
  // with nothing to tell a reader which was which. A dashboard that mixes
  // measurements with decoration cannot be used to decide anything.
  const fill = (id, key, vars) => { document.querySelector(id).textContent = t(dict, key, key, vars); };

  function paintStats() {
    const online = allCameras.filter(camera => camera.status !== 'offline').length;
    const offline = allCameras.filter(camera => camera.status === 'offline').length;
    const warning = allCameras.filter(camera => camera.status === 'warning').length;
    const throughput = allCameras.reduce((n, camera) => n + (camera.bitrate_kbps || 0), 0);

    document.querySelector('#stat-online').textContent = `${online} / ${allCameras.length}`;
    document.querySelector('#stat-alerts').textContent = String(warning + offline);
    document.querySelector('#stat-sites').textContent = String(rows.length);
    document.querySelector('#stat-throughput').textContent = throughput >= 1000
      ? `${(throughput / 1000).toFixed(1)} Mbps`
      : `${throughput} kbps`;
    fill('#sub-online', 'app.stat.sub.online', { offline });
    fill('#sub-alerts', 'app.stat.sub.alerts', { warning, offline });
    fill('#sub-sites', 'app.stat.sub.sites', { customers: fleet.customers.length });
    fill('#sub-throughput', 'app.stat.sub.throughput', { cameras: allCameras.length });
  }
  paintStats();
  const planTitle = document.querySelector('#plan-title');
  const planUsage = document.querySelector('#free-usage');
  const usageWrap = document.querySelector('#usage-wrap');
  const upgradeButton = document.querySelector('#upgrade-button');
  const entitlementLimit = edition.camera_limit == null ? 0 : Number(edition.camera_limit);
  const isCommunity = edition.edition === 'community';
  const isCommercialPaid = edition.edition === 'commercial' && edition.camera_limit == null;
  document.body.classList.toggle('edition-community', isCommunity);
  if (isCommunity) {
    planTitle.textContent = t(dict, 'app.communityPlan');
    planUsage.textContent = t(dict, 'app.communityUnlimited');
    usageWrap.hidden = true;
    upgradeButton.hidden = true;
  } else if (isCommercialPaid) {
    planTitle.textContent = t(dict, 'app.commercialPlan');
    planUsage.textContent = t(dict, 'app.commercialUnlimited');
    usageWrap.hidden = true;
    upgradeButton.hidden = true;
  } else {
    const freeLimit = entitlementLimit || Number(brand.freeTier?.cameras ?? 3);
    const freeUsed = Math.min(allCameras.length, freeLimit);
    planTitle.textContent = t(dict, 'app.freePlan');
    planUsage.textContent = t(dict, 'app.freeUsage', 'app.freeUsage', { used: freeUsed, freeCameras: freeLimit });
    document.querySelector('#free-usage-bar').style.width = `${freeLimit > 0 ? Math.min(100, (freeUsed / freeLimit) * 100) : 0}%`;
    // The marketing page is a separate deployment now, so this cannot be a
    // relative link. Only the hosted free plan ever reaches here — Community
    // and paid both hide the button above — but a dead link is still a bug.
    upgradeButton.addEventListener('click', () => { location.href = brand.pricingUrl || '/'; });
  }
  const limitNote = document.querySelector('#enrollment-limit-note');
  if (limitNote) {
    limitNote.textContent = isCommunity ? t(dict, 'app.onboarding.communityLimit')
      : isCommercialPaid ? t(dict, 'app.onboarding.commercialUnlimited')
      : t(dict, 'app.onboarding.freeLimit', 'app.onboarding.freeLimit', { freeCameras: entitlementLimit || Number(brand.freeTier?.cameras ?? 3) });
  }

  const body = document.querySelector('#fleet-body');
  function siteStatus(cameras) {
    if (cameras.some(camera => camera.status === 'offline')) return 'offline';
    if (cameras.some(camera => camera.status === 'warning')) return 'warning';
    return 'healthy';
  }
  function fmt(value, suffix = '') { return value == null ? '—' : `${typeof value === 'number' ? Math.round(value * 10) / 10 : value}${suffix}`; }
  function escapeHtml(value) { return String(value ?? '').replace(/[&<>'"]/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;',"'":'&#39;','"':'&quot;'}[char])); }

  function storagePluginId() {
    return plugins.find(plugin => plugin.reachable && plugin.manifest?.capabilities?.includes('storage_blob'))?.manifest?.id || null;
  }
  function aiPluginId() {
    return plugins.find(plugin => plugin.reachable && plugin.manifest?.capabilities?.includes('ai_analyze'))?.manifest?.id || null;
  }

  async function pollCommand(commandId, statusNode, labels = {}) {
    for (let attempt = 0; attempt < 70; attempt += 1) {
      const view = await tryJson(`api/v1/commands/${encodeURIComponent(commandId)}`, 3500);
      if (view.status === 'succeeded') return view.result;
      if (view.status === 'failed') throw new Error(view.result?.error || t(dict, labels.failed || 'app.command.failed'));
      statusNode.textContent = view.status === 'queued' ? t(dict, labels.queued || 'app.command.queued') : t(dict, labels.running || 'app.command.running');
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
    throw new Error(t(dict, labels.timeout || 'app.command.timeout'));
  }


  let activeLivePeer = null;
  const activeAiSnapshotUrls = new Set();

  function closeActiveLive() {
    if (activeLivePeer) {
      try { activeLivePeer.close(); } catch {}
      activeLivePeer = null;
    }
    const player = document.querySelector('#live-player');
    if (player) { player.srcObject = null; player.hidden = true; }
  }

  async function waitIceComplete(peer, timeoutMs = 12000) {
    if (peer.iceGatheringState === 'complete') return;
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => { cleanup(); reject(new Error(t(dict,'app.live.iceTimeout'))); }, timeoutMs);
      const changed = () => { if (peer.iceGatheringState === 'complete') { cleanup(); resolve(); } };
      const cleanup = () => { clearTimeout(timer); peer.removeEventListener('icegatheringstatechange', changed); };
      peer.addEventListener('icegatheringstatechange', changed);
    });
  }

  async function startLive(cameraId) {
    closeActiveLive();
    const player = document.querySelector('#live-player');
    const status = document.querySelector('#live-player-status');
    status.hidden = false; status.textContent = t(dict,'app.live.connecting');
    try {
      const rtc = await tryJson('api/v1/rtc/config', 3500);
      const iceServers = (rtc.ice_servers || []).map(server => ({
        urls: server.urls, username: server.username || '', credential: server.credential || '',
      }));
      const peer = new RTCPeerConnection({iceServers});
      activeLivePeer = peer;
      peer.addTransceiver('video', {direction:'recvonly'});
      peer.addEventListener('track', event => {
        player.srcObject = event.streams?.[0] || new MediaStream([event.track]);
        player.hidden = false; status.hidden = true;
        player.play().catch(() => {});
      });
      peer.addEventListener('connectionstatechange', () => {
        if (['failed','disconnected','closed'].includes(peer.connectionState)) {
          status.hidden = false;
          status.textContent = t(dict, peer.connectionState === 'failed' ? 'app.live.failed' : 'app.live.disconnected');
        }
      });
      const offer = await peer.createOffer();
      await peer.setLocalDescription(offer);
      await waitIceComplete(peer);
      const local = peer.localDescription;
      const response = await fetch(`api/v1/cameras/${encodeURIComponent(cameraId)}/live`, {
        method:'POST', headers:{'Content-Type':'application/json'},
        body:JSON.stringify({offer_sdp:local.sdp, offer_type:local.type, session_seconds:300}),
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const accepted = await response.json();
      const result = await pollCommand(accepted.command_id, status, {
        queued:'app.live.queued', running:'app.live.starting', failed:'app.live.failed', timeout:'app.live.timeout',
      });
      if (!result?.live?.sdp) throw new Error(t(dict,'app.live.noAnswer'));
      await peer.setRemoteDescription({type:result.live.sdp_type || 'answer', sdp:result.live.sdp});
      status.textContent = t(dict,'app.live.negotiating');
    } catch (error) {
      closeActiveLive();
      status.hidden = false;
      status.textContent = `${t(dict,'app.live.failed')}: ${error.message}`;
    }
  }

  async function browserSignedDownload(object) {
    const response = await fetch(`api/v1/plugins/${encodeURIComponent(object.storage_plugin_id)}/storage/downloads`, {
      method:'POST', headers:{'Content-Type':'application/json'},
      body:JSON.stringify({
        context:{camera_id:null}, object_ref:object.object_ref, expires_seconds:300, audience:'browser',
      }),
    });
    if (!response.ok) throw new Error(`storage HTTP ${response.status}`);
    return response.json();
  }

  async function renderAnalysis(cameraId, host, result) {
    const analysis = result?.analysis;
    if (!analysis) throw new Error(t(dict,'app.ai.noResult'));
    const transfer = await browserSignedDownload(analysis.snapshot);
    const snapshotResponse = await fetch(transfer.url, {headers:transfer.headers || {}});
    if (!snapshotResponse.ok) throw new Error(`snapshot HTTP ${snapshotResponse.status}`);
    const snapshotUrl = URL.createObjectURL(await snapshotResponse.blob());
    activeAiSnapshotUrls.add(snapshotUrl);
    const mode = analysis.metadata?.mode;
    const detections = analysis.detections || [];
    host.innerHTML = `
    <div class="ai-result-head"><strong>${escapeHtml(t(dict,'app.ai.result'))}</strong><small>${escapeHtml(analysis.model || analysis.ai_plugin_id || '')}</small></div>
    ${mode === 'simulated' ? `<div class="ai-simulated">${escapeHtml(t(dict,'app.ai.simulated'))}</div>` : ''}
    <div class="ai-frame"><img alt="AI snapshot" src="${escapeHtml(snapshotUrl)}" />
      ${detections.map(detection => detection.bbox ? `<div class="bbox" style="left:${Math.max(0,detection.bbox.x)*100}%;top:${Math.max(0,detection.bbox.y)*100}%;width:${Math.max(0,detection.bbox.width)*100}%;height:${Math.max(0,detection.bbox.height)*100}%"><span>${escapeHtml(detection.label)} ${Math.round((detection.confidence || 0)*100)}%</span></div>` : '').join('')}
    </div>
    <div class="ai-detections">${detections.length ? detections.map(d => `<span>${escapeHtml(d.label)} · ${Math.round((d.confidence || 0)*100)}%</span>`).join('') : escapeHtml(t(dict,'app.ai.empty'))}</div>`;
  }

  async function analyzeCamera(cameraId, button, status, host) {
    const ai = aiPluginId(); const storage = storagePluginId();
    if (!ai) { status.textContent = t(dict,'app.ai.pluginMissing'); return; }
    if (!storage) { status.textContent = t(dict,'app.archive.storageMissing'); return; }
    button.disabled = true; status.textContent = t(dict,'app.ai.queued');
    try {
      const response = await fetch(`api/v1/cameras/${encodeURIComponent(cameraId)}/analyze`, {
        method:'POST', headers:{'Content-Type':'application/json'},
        body:JSON.stringify({ai_plugin_id:ai, storage_plugin_id:storage, tasks:['person','vehicle']}),
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const accepted = await response.json();
      const result = await pollCommand(accepted.command_id, status, {
        queued:'app.ai.queued', running:'app.ai.running', failed:'app.ai.failed', timeout:'app.ai.timeout',
      });
      status.textContent = t(dict,'app.ai.ready');
      await renderAnalysis(cameraId, host, result);
    } catch (error) {
      status.textContent = `${t(dict,'app.ai.failed')}: ${error.message}`;
    } finally { button.disabled = false; }
  }

  async function loadCameraTimeline(cameraId, container) {
    container.replaceChildren();
    const title = document.createElement('div');
    title.className = 'timeline-head';
    title.innerHTML = `<strong>${escapeHtml(t(dict,'app.archive.timeline'))}</strong>`;
    container.appendChild(title);
    const list = document.createElement('div');
    list.className = 'timeline-list';
    container.appendChild(list);
    if (!isLive) {
      list.innerHTML = `<small>${escapeHtml(t(dict,'app.archive.liveRequired'))}</small>`;
      return;
    }
    try {
      const timeline = await tryJson(`api/v1/cameras/${encodeURIComponent(cameraId)}/recordings`, 3500);
      if (!timeline.recordings?.length) {
        list.innerHTML = `<small>${escapeHtml(t(dict,'app.archive.empty'))}</small>`;
        return;
      }
      for (const recording of timeline.recordings) {
        const durationMs = Math.max(0, new Date(recording.ended_at) - new Date(recording.started_at));
        const row = document.createElement('div');
        row.className = 'timeline-row';
        const when = new Date(recording.started_at).toLocaleString(locale, {dateStyle:'short', timeStyle:'medium'});
        row.innerHTML = `<div><strong>${escapeHtml(when)}</strong><small>${escapeHtml(recording.codec)} · ${recording.width}×${recording.height}</small></div><small>${escapeHtml(t(dict,'app.archive.duration','app.archive.duration',{seconds:(durationMs/1000).toFixed(1),segments:recording.segments?.length || 0}))}</small><button class="button small" data-play>${escapeHtml(t(dict,'app.archive.play'))}</button>`;
        row.querySelector('[data-play]').addEventListener('click', () => playRecording(recording.recording_id));
        list.appendChild(row);
      }
    } catch (error) {
      list.innerHTML = `<small>${escapeHtml(error.message)}</small>`;
    }
  }

  async function appendBuffer(sourceBuffer, bytes) {
    await new Promise((resolve, reject) => {
      const done = () => { cleanup(); resolve(); };
      const failed = () => { cleanup(); reject(new Error('MediaSource append failed')); };
      const cleanup = () => {
        sourceBuffer.removeEventListener('updateend', done);
        sourceBuffer.removeEventListener('error', failed);
      };
      sourceBuffer.addEventListener('updateend', done, {once:true});
      sourceBuffer.addEventListener('error', failed, {once:true});
      try { sourceBuffer.appendBuffer(bytes); } catch (error) { cleanup(); reject(error); }
    });
  }

  async function fetchMediaBytes(url, headers = {}) {
    const response = await fetch(url, {headers});
    if (!response.ok) throw new Error(`storage HTTP ${response.status}`);
    return new Uint8Array(await response.arrayBuffer());
  }

  let activeArchiveObjectUrl = null;
  async function playRecording(recordingId) {
    const video = document.querySelector('#archive-player');
    const status = document.querySelector('#archive-player-status');
    status.hidden = false;
    status.textContent = t(dict,'app.archive.loading');
    video.hidden = true;
    try {
      const manifest = await tryJson(`api/v1/recordings/${encodeURIComponent(recordingId)}/playback`, 6000);
      if (!('MediaSource' in window) || !MediaSource.isTypeSupported(manifest.mime_type)) {
        throw new Error(t(dict,'app.archive.unsupported'));
      }
      if (activeArchiveObjectUrl) {
        URL.revokeObjectURL(activeArchiveObjectUrl);
        activeArchiveObjectUrl = null;
      }
      const mediaSource = new MediaSource();
      const objectUrl = URL.createObjectURL(mediaSource);
      activeArchiveObjectUrl = objectUrl;
      video.src = objectUrl;
      await new Promise((resolve, reject) => {
        mediaSource.addEventListener('sourceopen', resolve, {once:true});
        mediaSource.addEventListener('error', reject, {once:true});
      });
      const sourceBuffer = mediaSource.addSourceBuffer(manifest.mime_type);
      await appendBuffer(sourceBuffer, await fetchMediaBytes(manifest.init_url, manifest.init_headers));
      for (const segment of manifest.segments) {
        await appendBuffer(sourceBuffer, await fetchMediaBytes(segment.url, segment.headers));
      }
      if (mediaSource.readyState === 'open') mediaSource.endOfStream();
      status.hidden = true;
      video.hidden = false;
      await video.play().catch(() => {});
    } catch (error) {
      status.hidden = false;
      status.textContent = `${t(dict,'app.archive.playError')}: ${error.message}`;
    }
  }

  function openSiteTelemetry(site) {
    const list = document.querySelector('#camera-telemetry-list');
    list.replaceChildren();
    const player = document.querySelector('#archive-player');
    player.pause(); player.removeAttribute('src'); player.load(); player.hidden = true;
    if (activeArchiveObjectUrl) { URL.revokeObjectURL(activeArchiveObjectUrl); activeArchiveObjectUrl = null; }
    const playerStatus = document.querySelector('#archive-player-status');
    playerStatus.hidden = false; playerStatus.textContent = t(dict,'app.archive.playerHint');
    for (const summary of site.cameras) {
      const camera = telemetryById.get(summary.id) || summary;
      const status = camera.status || 'warning';
      const item = document.createElement('article');
      item.className = 'telemetry-camera';
      item.innerHTML = `
      <div class="telemetry-camera-head"><div class="telemetry-camera-name">${escapeHtml(camera.name)}</div><span class="health-pill ${escapeHtml(status)}">${escapeHtml(t(dict, `app.${status}`))}</span></div>
      <div class="telemetry-grid">
        <div class="telemetry-metric"><span>${escapeHtml(t(dict,'app.telemetry.fps'))}</span><strong>${fmt(camera.fps)}</strong></div>
        <div class="telemetry-metric"><span>${escapeHtml(t(dict,'app.telemetry.bitrate'))}</span><strong>${fmt(camera.bitrate_kbps,' kbps')}</strong></div>
        <div class="telemetry-metric"><span>${escapeHtml(t(dict,'app.telemetry.codec'))}</span><strong>${escapeHtml(camera.codec || '—')}</strong></div>
        <div class="telemetry-metric"><span>${escapeHtml(t(dict,'app.telemetry.resolution'))}</span><strong>${camera.width && camera.height ? `${camera.width}×${camera.height}` : '—'}</strong></div>
        <div class="telemetry-metric"><span>${escapeHtml(t(dict,'app.telemetry.loss'))}</span><strong>${fmt(camera.packet_loss)}</strong></div>
        <div class="telemetry-metric"><span>${escapeHtml(t(dict,'app.telemetry.reconnects'))}</span><strong>${fmt(camera.reconnects)}</strong></div>
      </div>
      ${camera.last_error ? `<div class="telemetry-error">${escapeHtml(t(dict,'app.telemetry.error'))}: ${escapeHtml(camera.last_error)}</div>` : ''}
      <div class="camera-actions"><button class="button small primary" data-live-camera>${escapeHtml(t(dict,'app.live.start'))}</button><button class="button small" data-analyze>${escapeHtml(t(dict,'app.ai.analyze'))}</button><button class="button small" data-record>${escapeHtml(t(dict,'app.archive.record10'))}</button></div>
      <div class="recording-status" data-record-status></div>
      <div class="ai-result" data-ai-result hidden></div>
      <div class="timeline" data-timeline></div>`;
      const liveButton = item.querySelector('[data-live-camera]');
      const analyzeButton = item.querySelector('[data-analyze]');
      const recordButton = item.querySelector('[data-record]');
      const recordStatus = item.querySelector('[data-record-status]');
      const aiResult = item.querySelector('[data-ai-result]');
      const timeline = item.querySelector('[data-timeline]');
      const cameraId = camera.camera_id || camera.id;
      liveButton.disabled = !isLive;
      analyzeButton.disabled = !isLive;
      liveButton.addEventListener('click', () => startLive(cameraId));
      analyzeButton.addEventListener('click', async () => { aiResult.hidden = false; await analyzeCamera(cameraId, analyzeButton, recordStatus, aiResult); });
      if (!isLive) {
        recordButton.disabled = true;
        recordStatus.textContent = t(dict,'app.archive.liveRequired');
      } else if (!storagePluginId()) {
        recordButton.disabled = true;
        recordStatus.textContent = t(dict,'app.archive.storageMissing');
      } else {
        recordButton.addEventListener('click', async () => {
          recordButton.disabled = true; recordStatus.textContent = t(dict,'app.archive.queued');
          try {
            const response = await fetch(`api/v1/cameras/${encodeURIComponent(cameraId)}/recordings`, {
              method:'POST', headers:{'Content-Type':'application/json'},
              body:JSON.stringify({duration_seconds:10, segment_seconds:2, storage_plugin_id:storagePluginId()}),
            });
            if (!response.ok) throw new Error(`HTTP ${response.status}`);
            const accepted = await response.json();
            await pollCommand(accepted.command_id, recordStatus);
            recordStatus.textContent = t(dict,'app.archive.ready');
            await loadCameraTimeline(cameraId, timeline);
          } catch (error) {
            recordStatus.textContent = `${t(dict,'app.archive.failed')}: ${error.message}`;
          } finally { recordButton.disabled = false; }
        });
      }
      list.appendChild(item);
      loadCameraTimeline(cameraId, timeline);
    }
    openModal('#live-modal');
  }

  function render(filter = '') {
    const query = filter.trim().toLowerCase();
    body.replaceChildren();
    rows
      .filter(({ customer, site }) => !query || `${customer.name} ${site.name} ${site.city} ${site.cameras.map(c => c.name).join(' ')}`.toLowerCase().includes(query))
      .forEach(({ customer, site }) => {
        const status = siteStatus(site.cameras);
        const healthyCount = site.cameras.filter(c => c.status === 'healthy').length;
        const row = document.createElement('tr');
        const activity = site.cameras.map(c => new Date(c.last_seen)).sort((a,b) => b-a)[0] || new Date();
        row.innerHTML = `
        <td><div class="site-main"></div><div class="site-sub"></div></td>
        <td><span class="health-pill ${status}"></span></td>
        <td><strong>${healthyCount} / ${site.cameras.length}</strong><div class="metric-sub">${site.cameras.reduce((n,c) => n + (c.bitrate_kbps || 0), 0)} kbps</div></td>
        <td><span>${activity.toLocaleTimeString(locale, {hour:'2-digit',minute:'2-digit'})}</span><div class="metric-sub city-sub"></div></td>
        <td><button class="button small" data-live></button></td>`;
        row.querySelector('.site-main').textContent = site.name;
        row.querySelector('.site-sub').textContent = customer.name;
        row.querySelector('.city-sub').textContent = site.city;
        row.querySelector('.health-pill').textContent = t(dict, `app.${status}`);
        row.querySelector('[data-live]').textContent = t(dict, 'app.inspect');
        row.querySelector('[data-live]').addEventListener('click', () => openSiteTelemetry(site));
        body.appendChild(row);
      });
  }
  // The search box is the one piece of state a refresh must not stamp on, so
  // the current filter is kept rather than read back out of the input.
  let currentFilter = '';
  render();
  document.querySelector('#fleet-search').addEventListener('input', event => {
    currentFilter = event.target.value;
    render(currentFilter);
  });

  // Poll, because a dashboard that only tells the truth at page load is a
  // screenshot. Three rules, each for a reason that showed up in use:
  //
  //   - Skip while a modal is open. Repainting the fleet under someone who is
  //     reading a camera's telemetry or filling in the enrollment form is worse
  //     than showing them a number a few seconds old.
  //   - Skip ticks while the tab is hidden. Nobody is reading it, and a wall
  //     display left open for a week should not spend a week polling in the
  //     background. That test is on the tick rather than inside refresh().
  //   - A failed poll changes nothing. The old data stays on screen and the next
  //     tick tries again; blanking the fleet because one request timed out would
  //     turn a blip into an apparent outage.
  const REFRESH_MS = 5000;

  async function refresh() {
    // The modal guard lives here because it protects whoever is reading the
    // page, whatever caused the refresh. Whether the tab is worth polling at
    // all is a question about the tick, not about this function, so it is
    // asked below — an explicit refresh() should refresh.
    if (document.querySelector('.modal-backdrop.open')) return;
    let next;
    try {
      next = await Promise.all([loadFleet(), loadTelemetry(), loadGateways()]);
    } catch {
      return;
    }
    [fleet, telemetry, gateways] = next;
    telemetryById = new Map(telemetry.map(camera => [camera.camera_id, camera]));
    rows = fleet.customers.flatMap(customer => customer.sites.map(site => ({ customer, site })));
    allCameras = rows.flatMap(row => row.site.cameras);
    paintStats();
    render(currentFilter);
    renderGateways();
    markUpdated();
  }

  function markUpdated() {
    const stamp = new Date().toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
    sourceTag.textContent = `${t(dict, isLive ? 'app.liveData' : 'app.demo')} · ${stamp}`;
  }
  markUpdated();

  const timer = setInterval(() => { if (!document.hidden) refresh(); }, REFRESH_MS);
  // Node keeps a process alive for as long as an interval is pending, so a test
  // that starts the dashboard would hang on exit. `unref` releases that hold
  // without changing when the timer fires. Browsers return a plain number from
  // setInterval and have no unref, hence the optional calls.
  timer?.unref?.();
  // A tab coming back to the front should not wait out the rest of the interval.
  document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });

  function capabilityLabel(capability) {
    const key = {
      ai_analyze: 'app.plugins.cap.ai', storage_blob: 'app.plugins.cap.storage', event_sink: 'app.plugins.cap.events',
    }[capability];
    return key ? t(dict, key) : capability;
  }
  // The gateway is the thing that can take a whole site down, and until now it
  // was the one part of the fleet with no view. /api/v1/gateways has been
  // reporting uptime, version and per-status camera counts the whole time.
  function renderGateways() {
    const grid = document.querySelector('#gateways-grid');
    grid.replaceChildren();
    if (!Array.isArray(gateways) || gateways.length === 0) {
      const empty = document.createElement('p');
      empty.className = 'metric-sub';
      empty.textContent = t(dict, 'app.gateways.none');
      grid.appendChild(empty);
      return;
    }
    for (const gateway of gateways) {
      const healthy = Number(gateway.healthy_cameras || 0);
      const warn = Number(gateway.warning_cameras || 0);
      const off = Number(gateway.offline_cameras || 0);
      // A gateway is only as healthy as what it reports about. Offline cameras
      // are the loud case; warnings are the one that gets ignored.
      const status = off > 0 ? 'offline' : warn > 0 ? 'warning' : 'healthy';
      const card = document.createElement('article');
      card.className = 'plugin-card';
      card.innerHTML = `
        <div class="panel-head">
          <div><strong class="gw-name"></strong><div class="metric-sub gw-site"></div></div>
          <span class="health-pill ${status}"></span>
        </div>
        <div class="telemetry-grid">
          <div class="telemetry-metric"><span class="gw-l-uptime"></span><strong class="gw-uptime"></strong></div>
          <div class="telemetry-metric"><span class="gw-l-cameras"></span><strong class="gw-cameras"></strong></div>
          <div class="telemetry-metric"><span class="gw-l-version"></span><strong class="gw-version"></strong></div>
          <div class="telemetry-metric"><span class="gw-l-seen"></span><strong class="gw-seen"></strong></div>
        </div>`;
      const set = (sel, value) => { card.querySelector(sel).textContent = value; };
      set('.gw-name', gateway.hostname || gateway.gateway_id);
      set('.gw-site', gateway.site_id || '');
      set('.health-pill', t(dict, `app.${status}`));
      set('.gw-l-uptime', t(dict, 'app.gateways.uptime'));
      set('.gw-l-cameras', t(dict, 'app.gateways.cameras'));
      set('.gw-l-version', t(dict, 'app.gateways.version'));
      set('.gw-l-seen', t(dict, 'app.gateways.lastSeen'));
      set('.gw-uptime', formatUptime(gateway.uptime_seconds));
      set('.gw-cameras', `${healthy} / ${healthy + warn + off}`);
      set('.gw-version', gateway.version || '—');
      set('.gw-seen', gateway.sent_at ? new Date(gateway.sent_at).toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit' }) : '—');
      grid.appendChild(card);
    }
  }

  function formatUptime(seconds) {
    const total = Number(seconds);
    if (!Number.isFinite(total) || total < 0) return '—';
    const d = Math.floor(total / 86400);
    const h = Math.floor((total % 86400) / 3600);
    const m = Math.floor((total % 3600) / 60);
    if (d > 0) return `${d}d ${h}h`;
    if (h > 0) return `${h}h ${m}m`;
    return `${m}m`;
  }

  renderGateways();

  function renderPlugins() {
    const grid = document.querySelector('#plugins-grid');
    grid.replaceChildren();
    if (!plugins.length) {
      const empty = document.createElement('div');
      empty.className = 'plugin-card';
      empty.innerHTML = `<strong>${escapeHtml(t(dict,'app.plugins.emptyTitle'))}</strong><div class="plugin-description">${escapeHtml(t(dict,'app.plugins.emptyBody'))}</div>`;
      grid.appendChild(empty);
      return;
    }
    for (const plugin of plugins) {
      const manifest = plugin.manifest || {};
      const card = document.createElement('article');
      card.className = 'plugin-card';
      const caps = (manifest.capabilities || []).map(cap => `<span class="capability-pill">${escapeHtml(capabilityLabel(cap))}</span>`).join('');
      card.innerHTML = `
      <div class="plugin-head">
        <div class="plugin-icon">${manifest.capabilities?.includes('storage_blob') ? 'S3' : manifest.capabilities?.includes('ai_analyze') ? 'AI' : '✦'}</div>
        <div class="plugin-meta"><strong>${escapeHtml(manifest.name || manifest.id)}</strong><small>${escapeHtml(manifest.vendor || '')}${manifest.version ? ` · v${escapeHtml(manifest.version)}` : ''}</small></div>
        <i class="plugin-status ${plugin.reachable ? 'online' : ''}" title="${escapeHtml(plugin.reachable ? t(dict,'app.plugins.online') : t(dict,'app.plugins.offline'))}"></i>
      </div>
      <div class="plugin-description">${escapeHtml(manifest.description || '')}</div>
      <div class="capability-row">${caps}</div>
      <div class="plugin-actions"><small>${escapeHtml(plugin.reachable ? t(dict,'app.plugins.online') : t(dict,'app.plugins.offline'))}</small><button class="button small" data-plugin-test>${escapeHtml(t(dict,'app.plugins.test'))}</button></div>`;
      const button = card.querySelector('[data-plugin-test]');
      button.addEventListener('click', async () => {
        button.disabled = true; button.textContent = t(dict,'app.plugins.testing');
        try {
          const health = await tryJson(`api/v1/plugins/${encodeURIComponent(manifest.id)}/health`, 5000);
          button.textContent = health.status === 'ok' ? t(dict,'app.plugins.ok') : health.status;
        } catch { button.textContent = t(dict,'app.plugins.failed'); }
        setTimeout(() => { button.textContent = t(dict,'app.plugins.test'); button.disabled = false; }, 1800);
      });
      grid.appendChild(card);
    }
  }
  renderPlugins();
  document.querySelector('#plugins-link').addEventListener('click', () => setTimeout(() => document.querySelector('#plugins')?.scrollIntoView({behavior:'smooth'}), 0));
  document.querySelector('#reload-plugins').addEventListener('click', async event => {
    const button = event.currentTarget; button.disabled = true;
    try {
      const response = await fetch('api/v1/plugins/reload', {method:'POST'});
      if (!response.ok) throw new Error(String(response.status));
      plugins = await response.json(); renderPlugins();
    } catch { /* standalone demo has no API */ }
    finally { button.disabled = false; }
  });

  function openModal(selector) { document.querySelector(selector).classList.add('open'); }
  function closeModal(selector) { document.querySelector(selector).classList.remove('open'); }

  document.querySelector('#add-gateway').addEventListener('click', () => openModal('#onboarding-modal'));
  document.querySelectorAll('[data-close-modal]').forEach(node => node.addEventListener('click', () => closeModal('#onboarding-modal')));
  document.querySelectorAll('[data-close-live]').forEach(node => node.addEventListener('click', () => { closeActiveLive(); for (const url of activeAiSnapshotUrls) URL.revokeObjectURL(url); activeAiSnapshotUrls.clear(); closeModal('#live-modal'); }));

  const enrollmentForm = document.querySelector('#enrollment-form');
  const enrollmentResult = document.querySelector('#enrollment-result');
  const enrollmentError = document.querySelector('#enrollment-error');
  const enrollmentTokenNode = document.querySelector('#enrollment-token');
  const enrollmentExpiry = document.querySelector('#enrollment-expiry');
  const installCommandNode = document.querySelector('#install-command');
  const copyCommand = document.querySelector('#copy-command');
  const gatewayWaiting = document.querySelector('#gateway-waiting');
  const discoveryList = document.querySelector('#discovery-list');
  let currentInstallCommand = '';
  let currentGatewayId = '';
  let gatewayPoll = null;

  function slug(value) {
    const normalized = String(value || '').normalize('NFKD').replace(/[\u0300-\u036f]/g, '').toLowerCase();
    return normalized.replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '').slice(0, 36) || 'site';
  }
  function randomSuffix() {
    const bytes = new Uint8Array(4); crypto.getRandomValues(bytes);
    return [...bytes].map(value => value.toString(16).padStart(2, '0')).join('');
  }
  function apiBaseUrl() {
    return String(brand.gateway?.apiBaseUrl || location.origin).replace(/\/$/, '');
  }
  function buildInstallCommand({ enrollmentToken, gatewayId }) {
    const values = {
      apiUrl: apiBaseUrl(), enrollmentToken, gatewayId,
      cameraLimit: String(edition.camera_limit == null ? 0 : edition.camera_limit),
      image: brand.gateway?.image || 'vms-gateway:latest',
    };
    const template = brand.gateway?.installCommandTemplate || [
      'docker run --rm --network host \\',
      "  -e API_URL='{apiUrl}' \\",
      "  -e ENROLLMENT_TOKEN='{enrollmentToken}' \\",
      "  -e GATEWAY_ID='{gatewayId}' \\",
      "  -e CAMERA_LIMIT='{cameraLimit}' \\",
      "  -e CAMERA_USERNAME='admin' \\",
      "  -e CAMERA_PASSWORD='YOUR_CAMERA_PASSWORD' \\",
      '  {image}',
    ].join('\n');
    return template.replace(/\{(apiUrl|enrollmentToken|gatewayId|cameraLimit|image)\}/g, (_, key) => values[key]);
  }

  async function refreshEnrollmentStatus() {
    if (!currentGatewayId) return;
    try {
      const [gateways, cameras] = await Promise.all([tryJson('api/v1/gateways'), tryJson('api/v1/cameras')]);
      const gateway = gateways.find(item => item.gateway_id === currentGatewayId);
      const gatewayCameras = cameras.filter(camera => camera.gateway_id === currentGatewayId);
      gatewayWaiting.textContent = gateway
        ? t(dict, 'app.onboarding.connected').replace('{count}', String(gatewayCameras.length))
        : t(dict, 'app.onboarding.waiting');
      gatewayWaiting.classList.toggle('connected', Boolean(gateway));
      discoveryList.replaceChildren();
      for (const camera of gatewayCameras) {
        const item = document.createElement('div');
        item.className = 'discovery-item';
        item.innerHTML = `<div class="device-meta"><span>${escapeHtml(camera.name)}</span><small>${escapeHtml([camera.manufacturer, camera.model, camera.codec].filter(Boolean).join(' · ') || camera.camera_id)}</small></div><span class="health-pill ${escapeHtml(camera.status)}">${escapeHtml(t(dict, `app.${camera.status}`))}</span>`;
        discoveryList.appendChild(item);
      }
    } catch {
      gatewayWaiting.textContent = t(dict, 'app.onboarding.apiUnavailable');
    }
  }

  enrollmentForm.addEventListener('submit', async event => {
    event.preventDefault();
    enrollmentError.hidden = true;
    const submit = document.querySelector('#create-enrollment');
    submit.disabled = true;
    const data = new FormData(enrollmentForm);
    const customerName = String(data.get('customerName') || '').trim();
    const siteName = String(data.get('siteName') || '').trim();
    const city = String(data.get('city') || '').trim();
    const suffix = randomSuffix();
    currentGatewayId = `gw-${slug(siteName)}-${suffix}`;
    const payload = {
      customer_id: `${slug(customerName)}-${suffix}`, customer_name: customerName,
      site_id: `${slug(siteName)}-${suffix}`, site_name: siteName, city,
    };
    try {
      const response = await fetch('api/v1/enrollments', {
        method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(payload),
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const created = await response.json();
      enrollmentTokenNode.textContent = created.enrollment_token;
      enrollmentExpiry.textContent = t(dict, 'app.onboarding.expires').replace('{time}', new Date(created.expires_at).toLocaleTimeString(locale, {hour:'2-digit', minute:'2-digit'}));
      enrollmentResult.hidden = false;
      currentInstallCommand = buildInstallCommand({ enrollmentToken: created.enrollment_token, gatewayId: currentGatewayId });
      installCommandNode.textContent = currentInstallCommand;
      copyCommand.disabled = false;
      gatewayWaiting.textContent = t(dict, 'app.onboarding.waiting');
      discoveryList.replaceChildren();
      if (gatewayPoll) clearInterval(gatewayPoll);
      await refreshEnrollmentStatus();
      gatewayPoll = setInterval(refreshEnrollmentStatus, 2500);
    } catch (error) {
      enrollmentError.textContent = `${t(dict, 'app.onboarding.error')}: ${error.message}`;
      enrollmentError.hidden = false;
    } finally {
      submit.disabled = false;
    }
  });

  copyCommand.addEventListener('click', async event => {
    if (!currentInstallCommand) return;
    await navigator.clipboard?.writeText(currentInstallCommand);
    event.currentTarget.textContent = t(dict, 'app.onboarding.copied');
    setTimeout(() => event.currentTarget.textContent = t(dict, 'app.onboarding.copy'), 1400);
  });
  for (const modal of document.querySelectorAll('.modal-backdrop')) {
    modal.addEventListener('click', event => { if (event.target === modal) modal.classList.remove('open'); });
  }
  if (location.hash === '#onboarding') openModal('#onboarding-modal');

  // Runtime white-label preview editor.
  const brandModal = document.querySelector('#brand-modal');
  const brandForm = document.querySelector('#brand-form');
  const brandFields = {
    name: brand.name || '', legalName: brand.legalName || '', logoUrl: brand.logoUrl || '', faviconUrl: brand.faviconUrl || '',
    primary: brand.theme?.primary || '#6675f7', accent: brand.theme?.accent || '#23c6a8',
    background: brand.theme?.background || '#09111f', surface: brand.theme?.surface || '#0c1727',
    text: brand.theme?.text || '#eef4ff', muted: brand.theme?.muted || '#91a0b8', radius: brand.theme?.radius ?? 18,
    freeCameras: brand.freeTier?.cameras ?? 3, supportedLocales: (brand.supportedLocales || ['en']).join(', '),
    gatewayImage: brand.gateway?.image || '', apiBaseUrl: brand.gateway?.apiBaseUrl || '',
    installCommandTemplate: brand.gateway?.installCommandTemplate || '', customCssUrl: brand.customCssUrl || '',
  };
  Object.entries(brandFields).forEach(([name, value]) => { const input = brandForm.elements.namedItem(name); if (input) input.value = value; });
  document.querySelector('#brand-settings-link').addEventListener('click', event => { event.preventDefault(); brandModal.classList.add('open'); });
  document.querySelectorAll('[data-close-brand]').forEach(node => node.addEventListener('click', () => brandModal.classList.remove('open')));
  brandModal.addEventListener('click', event => { if (event.target === brandModal) brandModal.classList.remove('open'); });
  brandForm.addEventListener('submit', event => {
    event.preventDefault();
    const data = new FormData(brandForm);
    const locales = String(data.get('supportedLocales') || '').split(',').map(value => value.trim()).filter(Boolean);
    const override = {
      name: String(data.get('name') || '').trim() || brand.name, legalName: String(data.get('legalName') || '').trim(),
      logoUrl: String(data.get('logoUrl') || '').trim(), faviconUrl: String(data.get('faviconUrl') || '').trim(), customCssUrl: String(data.get('customCssUrl') || '').trim(),
      supportedLocales: locales.length ? locales : brand.supportedLocales,
      freeTier: { cameras: Math.max(0, Number(data.get('freeCameras') || 0)), sites: brand.freeTier?.sites ?? 1, requiresCard: brand.freeTier?.requiresCard ?? false },
      gateway: {
        image: String(data.get('gatewayImage') || '').trim(), apiBaseUrl: String(data.get('apiBaseUrl') || '').trim(),
        installCommandTemplate: String(data.get('installCommandTemplate') || ''),
      },
      theme: {
        primary: String(data.get('primary') || brand.theme?.primary), accent: String(data.get('accent') || brand.theme?.accent),
        background: String(data.get('background') || brand.theme?.background), surface: String(data.get('surface') || brand.theme?.surface),
        text: String(data.get('text') || brand.theme?.text), muted: String(data.get('muted') || brand.theme?.muted),
        radius: Math.max(0, Number(data.get('radius') || 0)),
      },
    };
    localStorage.setItem('brandOverride', JSON.stringify(override)); location.reload();
  });
  document.querySelector('#brand-reset').addEventListener('click', () => { localStorage.removeItem('brandOverride'); location.reload(); });
  if (location.hash === '#branding') brandModal.classList.add('open');

  // Handles worth reaching from a test. The rest close over local state that
  // only means anything mid-render. `stop` is here because anything that starts
  // this dashboard should be able to end it — a timer with no off switch is a
  // leak waiting for whoever embeds this next.
  return { siteStatus, fmt, escapeHtml, storagePluginId, aiPluginId, render, refresh, stop: () => clearInterval(timer) };
}
