# WebRTC live (prototype v6)

## Flow

```text
Browser createOffer
  -> Core API POST /api/v1/cameras/:id/live
  -> GatewayCommand::Live queued for the owning edge gateway
  -> edge gateway polls outbound over HTTPS
  -> RTSP H.264 (Retina, FrameFormat::SIMPLE)
  -> TrackLocalStaticSample (webrtc-rs)
  -> SDP answer returned through command result
  -> browser sets remote description
  -> SRTP media path via ICE/STUN/TURN
```

No inbound HTTP port is required on the customer LAN. Signaling is control-plane outbound polling. Media is peer-to-peer when ICE can establish it and uses TURN when a relay is required.

## The browser has to be able to receive H.264

The stream is passed through exactly as the camera encoded it — nothing is
transcoded — so a browser that cannot receive H.264 cannot watch, and no
configuration on this side changes that. Chrome, Edge and Safari have H.264.
**Firefox needs its OpenH264 plugin**, which is downloaded separately and does
not always arrive; `about:support` lists it under Media, and
`media.peerconnection.video.h264_enabled` has to be on. A Chromium built
without proprietary codecs has none either.

Such a browser is refused with a message saying so. Before 2026-09-27 it got
WebRTC's own "unable to populate media section, RTPSender created with no
codecs", which names neither H.264 nor the browser — that was the first thing
a real browser found here, and it cost half an hour of looking in the wrong
place.

Only `packetization-mode=1` is negotiated: the sender packs H.264 as STAP-A and
FU-A, so agreeing to single-NAL mode would be agreeing to send what the browser
then cannot decode. One profile (`42e01f`) is registered, which is enough —
codec matching falls back to the mime type, so a browser offering only
`42001f` or `640c1f` still negotiates and gets back the payload type it chose.
That is covered by a test against a real Chrome offer, kept in
`edge/gateway/tests/fixtures/`.

## The browser also has to offer a network path

The dashboard gathers ICE fully before it sends the offer, so whatever the
browser found is in that SDP. A browser that found nothing — no `a=candidate`
lines at all — has offered no path for the video, and the gateway can only wait
for a connection that cannot happen and give up twenty seconds later. The page
now says so instead of loading forever, and the gateway logs what arrived:
`the browser's offer candidates=2 host (2 mdns)`, or `candidates=none`.

**On Linux, the usual cause is mDNS.** Browsers hide local addresses behind
`<uuid>.local` names, and Chrome needs the mDNS port 5353 to register one. If
another service already holds 5353 — `avahi-daemon` does, on most desktop
distributions — Chrome can sit in `gathering` indefinitely and offer nothing.
Firefox on the same machine still offers `.local` candidates, which only help if
the gateway can resolve them, and it needs 5353 for that too.

Two ways out, and a relay is the reliable one:

- **Configure TURN** (`RTC_TURN_URLS`, `RTC_TURN_SECRET`). A relay candidate is
  an ordinary address, so it sidesteps mDNS entirely — and it is what a real
  site behind NAT needs anyway. `docs/TURN-DEPLOY.md`.
- **Free 5353**, by stopping the conflicting responder, so the browser can
  register its name and the gateway can look it up.

## The gateway has to find one too

The gateway gathers its own candidates before it answers, with a budget of
twelve seconds. What it does with that budget changed on 2026-10-21, because
the old shape refused sessions it could have carried.

It gathers from every interface the host has — one UDP socket each, one STUN
binding request from each — and on a box running Docker that includes every
bridge, which can reach nothing. The webrtc-rs pinned here then (0.20.3) never
finished gathering while one of those requests was outstanding: a STUN
transaction that timed out was logged and its client kept, and gathering was
reported complete only once every client was gone, so the event the session
waited for never arrived. A host with five bridges therefore spent the whole
budget and refused every session — with host candidates ready in the first
millisecond, a reflexive one from the server that did answer, and the browser
on the same machine. That was relaysight-vms-0kem, and an edge box running
Docker is the one the installer sets up. No budget would have fixed it: there
is no number that outlasts "never".

Upstream had fixed that mechanism before this host met it, and this build
carries the fix since 2026-10-22: webrtc 0.21.1 retires a STUN client on any
event, a timeout included (webrtc#910, off an unrelated dual-stack report,
webrtc#904). A socket with no route out therefore ends its transaction after
about eleven seconds — 10.8s measured — rather than holding gathering open
for ever. What follows is still what makes live view work on such a host:
eleven seconds is most of the budget, it is the same wait a server that
answers slowly costs, and the TURN half of the same bug is still open
upstream (relaysight-vms-6tf8).

Two things hold now.

**The session waits for kinds of candidate, not for sockets.** A `stun:` URL
can produce one server-reflexive candidate and a `turn:`/`turns:` URL one
relayed candidate; a second interface asking the same server produces the same
kind again. Once there is one of each kind the configuration can produce, the
gateway answers — there is nothing left to learn from a socket still
retransmitting into the dark. Host candidates are deliberately not on that
list: they come from local sockets, all of them queued before any server is
asked, so treating one as progress would risk answering before a second
interface's was added.

**The budget is a deadline, not a cliff.** When it does run out, the gateway
answers with the candidates in hand and logs what never arrived. It refuses
only when there is nothing at all to offer, and then it names the servers that
did not answer instead of saying "ICE gathering timeout" — which named the
symptom and nothing an operator could go and check. A session that has a path
is better answered late than refused; the operator waited either way.

An answer that was cut short says `a=end-of-candidates`. This answer is the
whole of the gateway's signalling — there is no channel to trickle a late
candidate down — and a browser not told otherwise waits for more before it
gives up on a pair.

Every session logs what each end brought: `the browser's offer candidates=4
host, 2 srflx`, then `the gateway's answer candidates=16 host, 4 srflx
gathering=Enough`. The last word is `Complete` (the library finished),
`Enough` (one of every kind, the rest abandoned) or `OutOfTime`.

**`RTC_UDP_ADDRS` pins which interfaces the gateway offers.** Comma-separated;
a bare address means any port on it; unset means every interface, which is what
it meant before the setting existed. Sixteen host candidates were measured on
the machine above, most of them Docker bridges no browser can reach — each one
a connectivity check the browser has to try, and a line in an SDP naming the
site's internal network. Pinned to one interface, the same session offered two
candidates and carried the same video.

## The wall

*Wall* in the sidebar is the other way to watch: a tile for every camera in the
fleet, in an order the operator sets by dragging one tile onto another, kept in
that browser. A double click gives one tile the screen. Tiles are built once per
camera and patched in place afterwards — the dashboard polls every five seconds,
and a grid that rebuilt itself on each tick would restart every stream twice a
minute.

**Streams start under a cap, one at a time.** This is the part to understand
before raising it. Opening a tile means the gateway dialling the camera, and
recorders are commonly sized for one or two readers: the NVR this was developed
against lost *every* camera for about ten minutes when three extra streams were
opened alongside the three it already had, and an unauthenticated `DESCRIBE`
went from 0.16s to 6.2s under that load (`docs/HARDWARE-NOTES.md`). "Show me all
sixteen at once" is that experiment. So the wall watches four at a time by
default, with a gap of just over a second between dials; the rest wait for a
slot and say on the tile that they are waiting. The cap is on the bar, and
*All* is there for a device that can take it.

A tile asks for the longest session the API grants — an hour, against the
modal's five minutes — because a wall is left up. When a session ends the tile
says so and frees its slot to a camera that was waiting. Nothing re-dials by
itself: a wall that retries a dead camera forever is the load above, on a
schedule.

*Record all* and *Save last 5 min of all* send the same two commands a single
camera's card sends, once per tile, each reporting in its own tile. They need a
storage plugin and the bar says so when there is none. Recording does not need
a tile to be watching — it is the gateway's own job, and a wall of dark tiles
can still be recording every camera on it.

## Current happy path

- H.264 only for zero-transcode live.
- Packetization mode 1, baseline-compatible SDP profile is registered by the gateway.
- Session lifetime is bounded (5 minutes from a camera card, an hour from a wall tile, max 1 hour in the API).
- Browser gathers ICE non-trickle before the command is sent.
- Gateway gathering has a twelve-second deadline and answers with whatever it
  holds, rather than refusing the session.

## Production requirements

- Configure `RTC_ICE_SERVERS_JSON` with at least one production TURN service. Public STUN alone is not reliable across enterprise NAT/CGNAT/firewalls.
- Use short-lived TURN credentials in production; the static JSON env is a prototype configuration surface.
- `RTC_STUN_URLS=` with no value means **no STUN**, not the default. It used to
  mean the default, so a deployment that asked for none was handed a public
  one; on an isolated network that is a server which never answers, and every
  session waits out its gathering budget before answering with host candidates
  alone.
- Pin the media interface with `RTC_UDP_ADDRS` on any host with interfaces a
  browser cannot reach — a box running Docker has one per bridge. They no
  longer cost a session its gathering budget, but their addresses are still in
  the SDP for the browser to try, and they are still the site's internal
  network written down in it.
- Add session authorization/RBAC before exposing beyond pilot users.
- Add viewer/session limits and media observability before large fan-out.
- H.265 requires transcoding or a browser-specific compatibility path and is deliberately out of the zero-transcode MVP.

## Is it working?

`make check-live` answers that without a camera and without a person watching:
ffmpeg publishes the committed H.264 fixture the way an encoder does, the real
gateway and control plane carry it, and a headless Firefox logs in, asks for
live video, answers the offer and reads `framesDecoded` off `getStats()`. It
fails unless a frame is decoded.

It needs ffmpeg, Firefox, and an OpenH264 plugin in one of this machine's
Firefox profiles — Firefox ships it disabled, and a fresh profile has not
downloaded it at all. It skips with a message saying so rather than failing
for the wrong reason. What it does not cover is the RTSP dial: ffmpeg can
receive RTSP but not serve it, so the fake camera here is a pushed stream.

`make check-live` has a second pass over the same stack, for the wall. It
serves the shipped dashboard — `app.html`, `dashboard-app.js`, `wall.js`, the
stylesheet — out of a copy with one module injected ahead of its own, which
logs in and keeps hold of the peer connections the page opens. It opens the
wall, switches it to a two-column layout, reads the column tracks and a tile's
box out of the browser's own layout engine, presses *Watch all*, and reads
`framesDecoded` off that tile's session. It fails unless a tile has a real box
with decoded frames in it — the half `web/tests` cannot reach, because jsdom
has no layout engine and no media stack, so a grid that collapsed to nothing
passes every test in there.

It earned its keep on the first run. Live view of a pushed stream sent the
FLV payload as it arrived — AVCC, length-prefixed, parameter sets out of band
— straight into a WebRTC sample writer that expects Annex B. A browser
received 288 KB with zero packet loss and decoded nothing. The RTSP path had
always asked for the framing it needs; the pushed path had one format and gave
it to both consumers.
