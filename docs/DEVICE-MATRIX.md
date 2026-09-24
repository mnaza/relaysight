# Cameras: what this has met, and what it has only been told about

One camera has ever been on the other end of this gateway: a Dahua NVR,
reached over the internet with its RTSP port forwarded, for more than twenty
hours (`docs/HARDWARE-NOTES.md`). Everything else on this page is either a
behaviour that turned up in that one device, or a behaviour this gateway was
written to survive because it is well documented elsewhere — and the second
kind has never been seen here.

This page exists so nobody has to guess which is which.

## Met, on a real device

| | |
| --- | --- |
| **Dahua NVR, RTSP** | 20 hours, 32 connectivity outages, recovered from every one |
| **Dahua SSRC, hex in `Transport` and decimal in `RTP-Info`** | stock retina refuses the stream; the gateway pins a fork with the fix ([scottlamb/retina#137](https://github.com/scottlamb/retina/pull/137)) |
| **Main and sub profiles, H.264, full-range colour** | the sub-stream is what live view uses, so a relayed session carries a fraction of the bytes |

## Handled, from the documented behaviour of other devices

Each of these is a real thing cameras do, each is pinned by a test, and none
has been seen on hardware here.

| | |
| --- | --- |
| **A SOAP fault with HTTP 200** | common when a password is wrong. The gateway reads the fault and repeats the camera's own words instead of reporting "no RTSP Uri" |
| **A stream URI naming the camera's own internal address** | typical of a Hikvision behind NAT. The gateway points the URI at the address the camera actually answered on, keeping port, path, query and any embedded credentials |
| **Credentials embedded in the returned stream URI** | moved out of the URL before use, and scrubbed from anything reported to the cloud |
| **Parameter sets only in the SDP, or only in band** | both paths are tested against a fake camera serving each way |
| **An encoding name in any case (`H264`, `h264`)** | matched case-insensitively |
| **A camera with one profile and no sub-stream** | live view uses the main stream rather than failing |
| **A camera that lists a sub-stream and will not hand out its URI** | live view falls back rather than having no live view at all |
| **No ONVIF at all** | `CAMERA_RTSP_URL`, or a source added from the dashboard |
| **ONVIF that multicast discovery cannot reach** | `ONVIF_HOSTS` names the addresses directly |

## Known not to be handled

- **Anything that is not H.264.** H.265 cameras are refused with a message
  saying so. Transcoding is not coming; it is what stops a site box keeping
  up.
- **Audio**, anywhere.
- **ONVIF Profile T events**, PTZ, and anything else beyond device
  information, profiles, stream and snapshot URIs.
- **Digest authentication over RTSP** is retina's to handle; WS-Security
  digest over ONVIF SOAP is handled here.

## If you have a camera

The most useful thing anyone can do with this list is make it shorter by
moving a row from the second table to the first. Point a gateway at a camera
and report what happened:

```bash
RELAYSIGHT_TEST_RTSP_URL=rtsp://user:pass@camera/stream \
  cargo test -p vms-gateway -- --ignored a_real_camera_produces_frames
ONVIF_HOSTS=192.168.1.50 cargo run -p vms-gateway     # and read the log
```

The first contact with the one device this has met found a bug that made the
gateway unusable with it. The second device will probably do the same, and
that is the point of writing this down rather than claiming a matrix.
