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

## Current happy path

- H.264 only for zero-transcode live.
- Packetization mode 1, baseline-compatible SDP profile is registered by the gateway.
- Session lifetime is bounded (default 5 minutes from the UI, max 1 hour in API).
- Browser gathers ICE non-trickle before the command is sent.

## Production requirements

- Configure `RTC_ICE_SERVERS_JSON` with at least one production TURN service. Public STUN alone is not reliable across enterprise NAT/CGNAT/firewalls.
- Use short-lived TURN credentials in production; the static JSON env is a prototype configuration surface.
- Add session authorization/RBAC before exposing beyond pilot users.
- Add viewer/session limits and media observability before large fan-out.
- H.265 requires transcoding or a browser-specific compatibility path and is deliberately out of the zero-transcode MVP.
