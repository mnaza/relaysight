# AI snapshot pipeline (prototype v6)

## Flow

```text
Dashboard Analyze
  -> Core API queues GatewayCommand::Analyze
  -> edge gateway calls ONVIF GetSnapshotUri URL
  -> HTTP Basic or Digest authentication
  -> snapshot uploaded directly to configured storage plugin (audience=edge)
  -> same small snapshot sent inline to AI plugin as base64
  -> normalized detections returned to gateway/core
  -> dashboard obtains audience=browser signed GET and draws bbox overlay
```

The AI plugin does not need direct connectivity to the camera or to the customer's LAN. It can be Python/CUDA/TensorRT, Rust, Go, or any HTTP service implementing Plugin Protocol v1.

## Demo adapter

The reference `ai-http-adapter` plugin, in the plugins repository, has three modes:

1. `UPSTREAM_AI_URL` configured: forwards requests to the user's model endpoint.
2. No upstream + `SIMULATED_AI=false`: safe no-op result.
3. No upstream + `SIMULATED_AI=true`: explicit synthetic detections for local UI demos. The UI labels these as simulated and they must never be presented as model output.

## Current limits

- Snapshot inference only; continuous video inference is a later edge-plugin placement.
- Snapshot payload limit is 8 MiB.
- Basic/Digest auth is supported. Camera/vendor quirks still need field testing.
- Detection bboxes are normalized `[0,1]` coordinates: x/y/width/height.
