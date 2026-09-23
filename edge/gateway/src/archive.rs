//! On-demand recording: a fixed number of seconds, as fMP4.
//!
//! The cutting is `segmenter`'s, which a ring buffer also uses. What belongs
//! here is knowing when to stop: at a keyframe at or after the requested
//! duration, so the recording ends on a GOP boundary.

use std::time::Duration;

use anyhow::{Context, anyhow};
use tokio::time::timeout;

use crate::segmenter::{ClosedSegment, Pushed, Segmenter, ticks_to_ms};

#[derive(Debug, Clone)]
pub struct CmafSegment {
    pub sequence: u32,
    pub start_offset_ms: u64,
    pub duration_ms: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CmafRecording {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub init: Vec<u8>,
    pub segments: Vec<CmafSegment>,
}

pub async fn record_h264_cmaf(
    raw_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    total_duration: Duration,
    target_segment_duration: Duration,
) -> anyhow::Result<CmafRecording> {
    let mut source =
        crate::frames::RtspSource::open(raw_url, username, password, crate::frames::Framing::Avcc)
            .await?;
    record_from(&mut source, total_duration, target_segment_duration).await
}

/// The recorder itself, over anything that yields H.264: an RTSP camera, a
/// pushed stream, a ring buffer being replayed.
pub(crate) async fn record_from(
    source: &mut dyn crate::frames::FrameSource,
    total_duration: Duration,
    target_segment_duration: Duration,
) -> anyhow::Result<CmafRecording> {
    let receive_deadline = total_duration + Duration::from_secs(12);
    let receive_started = tokio::time::Instant::now();
    let mut segmenter = Segmenter::new(target_segment_duration)?;
    let mut closed: Vec<ClosedSegment> = Vec::new();
    let mut first_timestamp: Option<i64> = None;

    while receive_started.elapsed() < receive_deadline {
        let frame = timeout(Duration::from_secs(4), source.next_frame())
            .await
            .context("frame timeout")??;
        let Some(frame) = frame else { break };

        match segmenter.push(&frame, source.parameters().as_ref())? {
            Pushed::Segment(segment) => closed.push(segment),
            Pushed::Nothing => {}
            Pushed::ParametersChanged => {
                return Err(anyhow!(
                    "camera changed H264 parameters during recording; start a new recording"
                ));
            }
        }
        if segmenter.pending_frames() == 0 {
            // Nothing decodable has arrived yet.
            continue;
        }
        let first = *first_timestamp.get_or_insert(frame.timestamp);

        let elapsed_ticks = frame.timestamp.saturating_sub(first).max(0) as u64;
        let elapsed = Duration::from_secs_f64(elapsed_ticks as f64 / frame.clock_rate as f64);
        // Stop at a keyframe at or after the requested duration, so the last
        // media segment closes on a GOP boundary without decoding anything.
        if elapsed >= total_duration && frame.keyframe && !closed.is_empty() {
            break;
        }
    }

    if let Some(segment) = segmenter.flush()? {
        closed.push(segment);
    }
    if closed.is_empty() {
        return Err(anyhow!("not enough H264 frames to build fMP4 recording"));
    }
    let clock_rate = segmenter
        .clock_rate()
        .ok_or_else(|| anyhow!("missing RTP clock rate"))?;
    let codec = segmenter
        .codec()
        .ok_or_else(|| anyhow!("missing H264 codec parameters"))?
        .to_owned();
    let (width, height) = segmenter
        .dimensions()
        .ok_or_else(|| anyhow!("missing video dimensions"))?;
    let first = first_timestamp.unwrap_or_default();
    let init = segmenter.init()?;

    let segments = closed
        .into_iter()
        .map(|segment| CmafSegment {
            sequence: segment.sequence,
            start_offset_ms: ticks_to_ms(
                segment.start_timestamp.saturating_sub(first).max(0) as u64,
                clock_rate,
            ),
            duration_ms: ticks_to_ms(segment.duration_ticks, clock_rate),
            bytes: segment.bytes,
        })
        .collect();

    Ok(CmafRecording {
        codec,
        width,
        height,
        init,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::record_from;
    use crate::frames::{Frame, VideoParameters, testing::ScriptedSource};
    use std::time::Duration;

    /// A minimal AVCDecoderConfigurationRecord: baseline, one SPS, one PPS,
    /// 4-byte lengths. (High profile would need a chroma_format the avcC box
    /// insists on, which is a camera-side matter, not the source's.)
    const AVCC: [u8; 19] = [
        1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xe0, 0x1e, 1, 0, 4, 0x68, 0xee, 0x3c,
        0x80,
    ];

    #[tokio::test]
    async fn records_from_a_source_that_is_not_a_camera() {
        // The recorder no longer knows what RTSP is. Anything that yields H.264
        // access units — a pushed stream, a test script — records the same way.
        let frames = (0..6).map(|i| Frame {
            data: bytes::Bytes::from(vec![0, 0, 0, 1, 0x65, i as u8]),
            timestamp: i * 3_000,
            clock_rate: 90_000,
            keyframe: i % 3 == 0,
            new_parameters: i == 0,
        });
        let mut source = ScriptedSource {
            frames: frames.collect(),
            parameters: Some(VideoParameters {
                rfc6381_codec: "avc1.42e01e".into(),
                pixel_dimensions: (1280, 720),
                extra_data: AVCC.to_vec(),
                frame_rate: Some((1, 30)),
            }),
        };
        let recording = record_from(
            &mut source,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .await
        .expect("a scripted source records");
        assert_eq!(recording.codec, "avc1.42e01e");
        assert_eq!((recording.width, recording.height), (1280, 720));
        assert!(!recording.init.is_empty(), "an init segment is written");
        assert_eq!(
            recording.segments.len(),
            2,
            "one segment per keyframe group at this target"
        );
        assert_eq!(recording.segments[0].start_offset_ms, 0);
        assert!(recording.segments.iter().all(|s| !s.bytes.is_empty()));
    }

    // ---- end to end, against the fake camera in `fake_camera.rs` ----

    #[tokio::test]
    async fn recording_a_real_stream_produces_playable_fragments() {
        // Exercises the whole archive path against genuine H.264: RTSP session,
        // depacketisation, avcC construction from in-band parameter sets, and
        // fMP4 segmenting. None of it was reachable before the fake camera.
        let camera = crate::fake_camera::FakeCamera::start(false).await.unwrap();
        let recording = super::record_h264_cmaf(
            &camera.url,
            None,
            None,
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("record from a reachable camera");

        assert_eq!(recording.width, 320, "dimensions come from the SPS");
        assert_eq!(recording.height, 240);
        assert!(
            recording.codec.starts_with("avc1."),
            "codec string must be an RFC 6381 one for the player, got {}",
            recording.codec
        );
        assert!(!recording.init.is_empty(), "no init segment");

        // The fixture is three seconds with a keyframe every half second, so a
        // one-second target must split it rather than emit a single blob.
        assert!(
            recording.segments.len() >= 2,
            "expected several segments, got {}",
            recording.segments.len()
        );
        assert!(recording.segments.iter().all(|s| !s.bytes.is_empty()));

        // Segments must run in order and abut, or the player seeks into gaps.
        for pair in recording.segments.windows(2) {
            assert!(
                pair[1].start_offset_ms >= pair[0].start_offset_ms,
                "segments out of order"
            );
            assert_eq!(
                pair[1].sequence,
                pair[0].sequence + 1,
                "sequence must be dense"
            );
        }
    }

    #[tokio::test]
    async fn recording_reports_failure_rather_than_an_empty_recording() {
        // A camera that refuses the session must surface as an error; an empty
        // recording would be stored as a successful one.
        let camera = crate::fake_camera::FakeCamera::start(true).await.unwrap();
        assert!(
            super::record_h264_cmaf(
                &camera.url,
                None,
                None,
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(1),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn recording_works_whichever_way_the_camera_supplies_parameter_sets() {
        // The archive writer builds an avcC box from SPS and PPS. Where those come
        // from differs by vendor, and a writer that silently depends on one source
        // records nothing playable against half the cameras in the field.
        for mode in [
            crate::fake_camera::ParameterSets::Both,
            crate::fake_camera::ParameterSets::SdpOnly,
            crate::fake_camera::ParameterSets::InBandOnly,
        ] {
            let camera = crate::fake_camera::FakeCamera::start_with(false, mode)
                .await
                .unwrap();
            let recording = super::record_h264_cmaf(
                &camera.url,
                None,
                None,
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap_or_else(|e| panic!("{mode:?} camera failed to record: {e:#}"));

            assert_eq!(
                recording.width, 320,
                "{mode:?}: dimensions come from the SPS"
            );
            assert_eq!(recording.height, 240, "{mode:?}");
            assert!(
                !recording.init.is_empty(),
                "{mode:?}: no init segment, so no avcC"
            );
            assert!(!recording.segments.is_empty(), "{mode:?}: recorded nothing");
        }
    }
}
