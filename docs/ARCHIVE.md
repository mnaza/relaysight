# Archive / recording pipeline

This slice adds short on-demand H.264 recordings using fragmented MP4 (fMP4/CMAF-style media segments) without transcoding.

## Data path

```text
Browser
  | POST /cameras/:id/recordings
  v
Core API -- queues outbound command --> Gateway
                                      |
                                      | RTSP H.264 (Retina, FrameFormat::MP4)
                                      v
                                fMP4 segmenter
                                      |
                                      | asks Core API for signed PUT URLs
                                      v
                              Storage plugin
                                      |
                                      | signed PUT (media bypasses Core API)
                                      v
                             S3 / MinIO / B2

Core API stores only the recording manifest/index.
```

The gateway starts on an H.264 random-access frame, creates one initialization segment (`init.mp4`) and one or more media segments (`seg-xxxxx.m4s`). Segment boundaries are aligned to keyframes after the target segment duration is reached.

## Playback

`GET /api/v1/recordings/:recording_id/playback` resolves each stored object through the configured storage plugin and returns short-lived signed GET URLs plus the RFC 6381 codec string. The dashboard uses MediaSource Extensions and appends the init segment followed by media segments in sequence.

For browser playback, the storage origin must allow CORS for `GET` / `HEAD`. The included MinIO dev profile configures bucket CORS automatically.

## API

```text
POST /api/v1/cameras/:camera_id/recordings
GET  /api/v1/cameras/:camera_id/recordings
GET  /api/v1/commands/:command_id
GET  /api/v1/recordings/:recording_id/playback
```

The gateway uses authenticated outbound polling only:

```text
GET  /api/v1/gateways/:gateway_id/commands/next
POST /api/v1/gateways/:gateway_id/commands/:command_id/complete
```

No inbound port is required on the customer LAN.

Example request:

```json
{
  "duration_seconds": 10,
  "segment_seconds": 2,
  "storage_plugin_id": "storage-s3"
}
```

## Retention

`DEFAULT_RETENTION_DAYS` defaults to `30`. The API applies `delete_after` server-side when a gateway completes a recording. A prototype retention worker periodically calls the same storage plugin's delete capability for the init segment and all media segments. Set `DEFAULT_RETENTION_DAYS=0` to disable automatic deletion.

Production should persist retention policies per organization/site/camera rather than using one process-wide default.

## Current limitations

- H.264 only for this zero-transcode path.
- Archive index and command queue are in memory and disappear when the API restarts.
- Audio is not muxed yet.
- Streams that change H.264 parameters mid-recording are deliberately rejected; a new init segment/recording boundary is required.
- Cameras using B-frames need explicit decode/composition timestamp handling before being declared fully supported.
- This is on-demand short recording, not continuous rolling recording yet.
- External object storage must expose suitable browser CORS for direct playback.

These constraints keep the first recording path deterministic while preserving the architecture needed for continuous recording later.
