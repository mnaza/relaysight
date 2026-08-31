# TURN — what it costs, and what actually decides it

Worked 2026-08-29. The conclusion reverses the assumption it started from.

## Why this document exists

Live video goes gateway → browser directly over WebRTC, and the cloud never touches the
media. That is the decision the whole economic case rests on. It holds until the
customer's network is behind symmetric NAT or blocks UDP, at which point the session
needs a TURN relay — and a TURN relay carries the full stream through your server, which
is exactly the cost the architecture was designed to avoid.

**TURN is not implemented.** Without it those sessions do not degrade, they fail.

## The thing to understand before any number

**TURN need is a property of the site, not of the session.** A network either permits a
direct path or it does not. If a customer's firewall blocks UDP, every session from that
site relays, forever. So "10–15% of WebRTC sessions need TURN", the figure usually quoted
from consumer deployments, is the wrong model here: plan for a *fraction of sites at
100%*, not all sites at 15%. And premises that install security cameras skew toward
restrictive egress rules, so that fraction is higher than consumer averages, not lower.

Capacity planning follows from the number of relaying **sites**, not sessions.

## Assumptions

Typical, not measured — no camera has ever been connected to this code.

| | bitrate |
|---|---|
| Main stream, 1080p @ 15 fps H.264 | 4 Mbit/s |
| Substream, D1/720p | 0.7 Mbit/s |

A relay costs one unit of billable egress per viewer (ingress from the gateway is
normally free). Two people watching the same camera cost twice.

## Per site, per month

| Usage pattern | main stream | substream |
|---|---|---|
| On demand — 20 cameras, 30 min of viewing a day | 27 GB | 5 GB |
| Alarm monitoring — 4 cameras watched continuously | **5,184 GB** | 907 GB |

**On-demand viewing is a rounding error.** 27 GB a month is nothing on any host. The
entire question is the monitoring tier — the always-on video wall, which is a product
3dEYE sells and which is where continuous relay lives.

## What that costs, and this is the whole finding

One alarm-monitoring site, main stream, 5.18 TB a month:

| Host | Cost |
|---|---|
| Hetzner dedicated, €97/mo, unmetered 1 Gbit | **€0 marginal** — the box is flat-rate |
| Hetzner Cloud, 20 TB included then €1/TB | **€0** — one site fits inside the allowance |
| AWS egress at $0.09/GB | **$467 per month, for one site** |

Against a plan priced near 3dEYE's entry $200/month, AWS egress alone is more than twice
the revenue. On Hetzner the same traffic is free at the margin.

**So TURN does not decide whether this business works. The hosting decision does, and the
gap between the two answers is roughly a hundredfold.** The original worry — that video
makes a solo-built VSaaS unaffordable — is true on a hyperscaler and false on flat-rate
bandwidth.

## Capacity on flat-rate hosting

With a flat-rate box the binding constraint is not monthly volume, it is **concurrent
bandwidth**. Allowing 500 Mbit/s of a 1 Gbit uplink as usable:

| Relaying monitoring sites, 4 cameras each | per site | sites per €97 box |
|---|---|---|
| main stream | 16 Mbit/s | **31** |
| substream | 2.8 Mbit/s | **179** |

At $100/month per site that is $3,125 of revenue against €97 of relay cost — **3.1%** on
the main stream, **0.5%** on the substream. On-demand-only sites are not a constraint at
all; hundreds fit.

## The lever that is currently pulled the wrong way

`edge/gateway/src/onvif.rs:185` sorts the camera's ONVIF media profiles by descending
pixel count and takes the first:

```rust
profiles.sort_by_key(|p| Reverse(p.width.unwrap_or(0) as u64 * p.height.unwrap_or(0) as u64));
let profile = profiles.remove(0);
```

**Every path therefore uses the highest-resolution stream the camera offers, including
live preview.** Almost all ONVIF cameras publish a substream precisely for this purpose.

**Implemented 2026-08-29.** `select_profiles` in `onvif.rs` now returns a pair: the
highest-resolution profile for recording, and the best substream at or below 1280×720 for
live. Discovery fetches a stream URI for each, `CameraSource` carries both, and only
`live::start_h264` uses the live one — `archive::record_h264_cmaf` still records the main
stream.

Three cases the policy has to survive, each pinned by a test:

- **The codec must be carriable.** Live is H.264 passthrough, so an MJPEG or H.265
  substream would negotiate and then deliver nothing. Those profiles are excluded, and a
  camera with no usable substream falls back to the main stream rather than failing.
- **A profile can be a second main stream.** A 1600×1200 profile alongside a 2592×1944 one
  is not a preview; relaying it saves almost nothing. Hence the ceiling, above which the
  next-best candidate is taken only if nothing fits under it.
- **Encoding is often absent** from `GetProfiles`. Treating that as unusable would leave
  the substream unused on hardware that supports it, so unknown encoding is accepted; the
  RTSP session fails loudly if it turns out to be wrong.

Discovery logs which pair it chose, at INFO, including both resolutions. **That line is
how you confirm in the field that the 5.7× is actually being taken** — a camera that
quietly offers no substream looks identical from the outside.

## What is not yet known

- Real camera bitrates vary by more than the 5.7× this analysis turns on. The figures
  above are typical values, not measurements. **Measure before committing to a price.**
- What fraction of real customer sites will relay. Unknowable without deployments;
  instrument it from the first pilot, per site, and treat the number as a headline metric.
- Hetzner's unmetered uplink is subject to fair use. 500 Mbit/s sustained on a €97 box is
  an assumption that needs confirming with them before it becomes a capacity plan.
- Whether TURN should be self-hosted (coturn) or bought. At these volumes self-hosting on
  the same flat-rate box is obviously cheaper; that stops being true if relay share is
  far above the planning figure.

## The order to do things

1. Instrument relay share per site from the first pilot. It is the number everything else
   depends on and it cannot be guessed.
2. ~~Decide the profile policy above~~ — done 2026-08-29. Confirm against real cameras
   that the substream is being selected, using the discovery log line.
3. Stand up coturn on flat-rate hosting. Do not put the relay on metered egress.

## Credentials, implemented 2026-08-31

A relay costs bandwidth, which makes its credentials worth stealing, and the ICE
configuration is sent to the browser by design. A fixed username and password there is
the same as publishing them, and the relay becomes an open proxy billed to us.

So they are derived instead: username is `<expiry>:<label>`, password is
`base64(HMAC-SHA1(secret, username))`. coturn validates this in `use-auth-secret` mode
and stores nothing. A leaked pair expires in ten minutes by default and nothing has to be
revoked. The label is the gateway id, so a relay log ties back to a site without a second
lookup.

`RTC_TURN_URLS` and `RTC_TURN_SECRET` enable it. Both are required — URLs without a
secret are treated as no relay at all, because a browser that tries a relay it cannot
authenticate to spends the whole ICE timeout doing it, which is worse than not offering
one. The API warns at startup when no relay is configured, since the failure it causes is
otherwise silent: sessions that never start, with nothing in the logs to point at.

A test asserts the secret does not appear in the serialised ICE configuration. That is
the one mistake in this file which would be invisible in review.
