# What real cameras do

Everything in this repository that talks RTSP was developed against
`fake_camera`, which answers the five methods the gateway sends and nothing
else. That is enough to test our logic and it cannot tell us what a real
encoder does, which is the half that goes wrong.

This file records what real hardware actually did, one device at a time. Add to
it when you point the gateway at something new.

## Dahua NVR, two channels, H.264

Reached over the internet with only the RTSP port forwarded. Streams at
`/cam/realmonitor?channel=N&subtype=M`, main stream `subtype=0`, substream
`subtype=1`.

| | main | substream |
| --- | --- | --- |
| resolution | 1280x720 | 352x288 |
| H.264 profile | High | Baseline |
| frame rate | 25 | 25 |
| B-frames | none | none |
| colour | `yuvj420p`, full range | same |

Two things worth keeping.

**The substream is CIF.** `select_profiles` will pick it for live view, which is
what we want for TURN bandwidth, but 352x288 is the floor of usable. A site with
this hardware gets a live view that is legible and not much more.

**Full-range colour.** `color_range=pc`, not the `tv` most pipelines assume.
Passthrough does not care. Anything that decodes — snapshots, an AI plugin —
will crush or wash the levels if it assumes limited range.

### The bug this found: a decimal SSRC kills the session

The gateway could not talk to this device at all. It failed at PLAY:

```
200 response to PLAY CSeq=4: Unparseable ssrc 4294938338
```

The device writes the same SSRC two different ways in one session:

```
SETUP  -> Transport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=A9556386
PLAY   -> RTP-Info:  url=trackID=0;seq=13279;rtptime=...;ssrc=2840945542
```

`0xA9556386 == 2840945542`. Hex in `Transport`, decimal in `RTP-Info`.

RFC 2326 fixes the radix for `ssrc` in the Transport header — section 12.39,
eight hex digits. It says nothing about the radix in RTP-Info, section 12.33.
Dahua read that gap the other way, and it is not clearly wrong to.

`retina` parses both as hex and returns a hard error on failure, so a session
that set up cleanly dies at PLAY. Note that `rtptime`, two match arms above the
one that fails, warns and continues for exactly the same class of problem.

**Status: patched locally, not upstream yet.** Hex first, then decimal, and warn
instead of failing when neither parses. Until that lands, a build that has to
talk to Dahua needs the patch.

### What we could not test here

ONVIF. Only the RTSP port is forwarded, so `GetProfiles`, `GetStreamUri` and
device information were unreachable, and WS-Discovery is multicast and does not
cross a routed boundary anyway.

That leaves a real gap in the gateway, which this device made obvious. There are
two ways in and nothing between them: multicast discovery, which needs the
camera on the same segment, or `CAMERA_RTSP_URL`, which skips ONVIF entirely and
gives up profile selection, the substream and snapshots. **A camera reachable by
address but not by multicast — a separate VLAN, a VPN, a port forward — has no
good path.** An `ONVIF_HOSTS` list that skips discovery and calls
`resolve_camera` directly would close it; the code underneath already takes an
address.

## Running the real-camera test

```text
RELAYSIGHT_TEST_RTSP_URL='rtsp://user:pass@host:554/path' \
  cargo test -p vms-gateway -- --ignored --nocapture real_camera
```

The URL carries a password, so it is read from the environment and never
printed. Only the redacted endpoint appears in the output, which also exercises
`redacted_endpoint` against a real credentialed URL rather than a synthetic one.

Expect packet loss over the internet. One channel here reported 181 lost packets
at 11 fps and the other 0 at 20 fps, on the same device in the same minute. That
is the link, not the code, and the telemetry reporting it is the point.
