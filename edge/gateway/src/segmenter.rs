//! Frames in, fMP4 media segments out, one at a time.
//!
//! A recording with an end knows all its frames before it writes anything. A
//! ring buffer never ends, so it cannot: it has to hand over each segment as
//! the stream closes it. Both want the same segments — cut on a keyframe at or
//! after the target duration, so a segment is independently decodable — so
//! both use this.
//!
//! A frame's duration is only known when the next frame arrives, which is why
//! a segment is emitted by the keyframe that ends it rather than by the last
//! frame inside it.
//! See docs/superpowers/specs/2026-09-23-recording-policies-design.md.

use std::{num::NonZeroU32, time::Duration};

use anyhow::{Context, anyhow};
use shiguredo_mp4::{
    TrackKind, Uint,
    boxes::{Avc1Box, AvccBox, SampleEntry, VisualSampleEntryFields},
    mux::{Fmp4SegmentMuxer, Sample},
};

use crate::frames::{Frame, VideoParameters};

#[derive(Debug)]
pub(crate) struct EncodedFrame {
    pub timestamp: i64,
    pub data: Vec<u8>,
    pub keyframe: bool,
}

#[derive(Debug)]
pub(crate) struct AvccConfig {
    pub profile: u8,
    pub compatibility: u8,
    pub level: u8,
    pub length_size_minus_one: u8,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

/// One media segment, and where it sits in the stream it came from.
#[derive(Debug, Clone)]
pub(crate) struct ClosedSegment {
    pub sequence: u32,
    /// The source's own timestamp for the segment's first frame.
    pub start_timestamp: i64,
    pub duration_ticks: u64,
    pub bytes: Vec<u8>,
}

/// What a frame did.
#[derive(Debug)]
pub(crate) enum Pushed {
    /// Held, nothing closed.
    Nothing,
    /// This frame closed the segment before it.
    Segment(ClosedSegment),
    /// The stream changed shape. The frame was **not** taken: flush, start a
    /// new segmenter, and offer it again.
    ParametersChanged,
}

pub(crate) struct Segmenter {
    target_ticks: i64,
    muxer: Fmp4SegmentMuxer,
    clock_rate: Option<NonZeroU32>,
    target: Duration,
    codec: Option<String>,
    dimensions: Option<(u32, u32)>,
    sample_entry: Option<SampleEntry>,
    /// A frame duration to fall back on when the clock does not advance.
    fallback_duration: Option<u32>,
    pending: Vec<EncodedFrame>,
    sequence: u32,
}

impl Segmenter {
    pub fn new(target_segment_duration: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            target_ticks: 0,
            muxer: Fmp4SegmentMuxer::new()?,
            clock_rate: None,
            target: target_segment_duration,
            codec: None,
            dimensions: None,
            sample_entry: None,
            fallback_duration: None,
            pending: Vec::new(),
            sequence: 0,
        })
    }

    pub fn codec(&self) -> Option<&str> {
        self.codec.as_deref()
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }

    pub fn clock_rate(&self) -> Option<NonZeroU32> {
        self.clock_rate
    }

    /// The init segment. Only meaningful once a segment has been produced:
    /// the muxer learns the track from the samples it was given.
    pub fn init(&mut self) -> anyhow::Result<Vec<u8>> {
        self.muxer.init_segment_bytes().map_err(Into::into)
    }

    /// How far into the current segment the stream has got.
    pub fn pending_frames(&self) -> usize {
        self.pending.len()
    }

    pub fn push(
        &mut self,
        frame: &Frame,
        parameters: Option<&VideoParameters>,
    ) -> anyhow::Result<Pushed> {
        // A segment has to start at a random access point to be decodable on
        // its own, and so does a recording.
        if self.pending.is_empty() && self.sample_entry.is_none() && !frame.keyframe {
            return Ok(Pushed::Nothing);
        }

        if self.sample_entry.is_none() || frame.new_parameters {
            let parameters = parameters
                .ok_or_else(|| anyhow!("video parameters unavailable after H264 frame"))?;
            let avcc = parse_avcc(&parameters.extra_data)?;
            let changed = self.sample_entry.is_some()
                && (self.codec.as_deref() != Some(parameters.rfc6381_codec.as_str())
                    || self.dimensions != Some(parameters.pixel_dimensions));
            if changed {
                return Ok(Pushed::ParametersChanged);
            }
            let (width, height) = parameters.pixel_dimensions;
            self.sample_entry = Some(create_avc1_sample_entry(width, height, &avcc)?);
            self.codec = Some(parameters.rfc6381_codec.clone());
            self.dimensions = Some(parameters.pixel_dimensions);
            self.fallback_duration = parameters.frame_rate.and_then(|(num, den)| {
                if den == 0 {
                    None
                } else {
                    let rate = frame.clock_rate as f64;
                    Some(((rate * num as f64 / den as f64).round() as u32).max(1))
                }
            });
        }

        match self.clock_rate {
            None => {
                let rate = NonZeroU32::new(frame.clock_rate)
                    .ok_or_else(|| anyhow!("missing RTP clock rate"))?;
                self.clock_rate = Some(rate);
                self.target_ticks = (self.target.as_secs_f64() * rate.get() as f64)
                    .round()
                    .max(1.0) as i64;
            }
            Some(rate) if rate.get() != frame.clock_rate => {
                return Ok(Pushed::ParametersChanged);
            }
            Some(_) => {}
        }

        // A keyframe at or after the target closes the segment before it, and
        // starts the next one.
        let closes = frame.keyframe
            && self.pending.first().is_some_and(|first| {
                frame.timestamp.saturating_sub(first.timestamp) >= self.target_ticks
            });
        let closed = if closes {
            Some(self.close(Some(frame.timestamp))?)
        } else {
            None
        };

        self.pending.push(EncodedFrame {
            timestamp: frame.timestamp,
            data: frame.data.to_vec(),
            keyframe: frame.keyframe,
        });

        Ok(match closed {
            Some(segment) => Pushed::Segment(segment),
            None => Pushed::Nothing,
        })
    }

    /// Close whatever is pending, because the stream ended.
    pub fn flush(&mut self) -> anyhow::Result<Option<ClosedSegment>> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        self.close(None).map(Some)
    }

    /// Build a media segment out of the pending frames. `next_timestamp` is
    /// the frame that comes after them, which is what gives the last one its
    /// duration; without it, the last frame borrows the one before.
    fn close(&mut self, next_timestamp: Option<i64>) -> anyhow::Result<ClosedSegment> {
        let clock_rate = self
            .clock_rate
            .ok_or_else(|| anyhow!("missing RTP clock rate"))?;
        let sample_entry = self
            .sample_entry
            .clone()
            .ok_or_else(|| anyhow!("missing H264 parameters"))?;
        let frames = std::mem::take(&mut self.pending);
        let start_timestamp = frames
            .first()
            .map(|frame| frame.timestamp)
            .ok_or_else(|| anyhow!("no frames to close a segment with"))?;

        let mut last_good = self
            .fallback_duration
            .unwrap_or_else(|| (clock_rate.get() / 25).max(1));
        let mut durations = Vec::with_capacity(frames.len());
        for index in 0..frames.len() {
            let next = frames
                .get(index + 1)
                .map(|frame| frame.timestamp)
                .or(next_timestamp);
            if let Some(next) = next {
                let delta = next.saturating_sub(frames[index].timestamp);
                if delta > 0 {
                    last_good = u32::try_from(delta).unwrap_or(u32::MAX).max(1);
                }
            }
            durations.push(last_good);
        }

        let mut samples = Vec::with_capacity(frames.len());
        let mut data_offset = 0_u64;
        let mut duration_ticks = 0_u64;
        for (frame, duration) in frames.iter().zip(&durations) {
            samples.push(Sample {
                track_kind: TrackKind::Video,
                timescale: clock_rate,
                sample_entry: Some(sample_entry.clone()),
                duration: *duration,
                keyframe: frame.keyframe,
                composition_time_offset: None,
                data_offset,
                data_size: frame.data.len(),
            });
            data_offset = data_offset.saturating_add(frame.data.len() as u64);
            duration_ticks = duration_ticks.saturating_add(u64::from(*duration));
        }

        let mut bytes = self.muxer.create_media_segment_metadata(&samples)?;
        bytes.reserve(data_offset as usize);
        for frame in &frames {
            bytes.extend_from_slice(&frame.data);
        }

        let sequence = self.sequence;
        self.sequence += 1;
        Ok(ClosedSegment {
            sequence,
            start_timestamp,
            duration_ticks,
            bytes,
        })
    }
}

pub(crate) fn ticks_to_ms(ticks: u64, clock_rate: NonZeroU32) -> u64 {
    ((ticks as u128 * 1000) / clock_rate.get() as u128) as u64
}

pub(crate) fn create_avc1_sample_entry(
    width: u32,
    height: u32,
    avcc: &AvccConfig,
) -> anyhow::Result<SampleEntry> {
    let width = u16::try_from(width).context("video width exceeds MP4 avc1 field")?;
    let height = u16::try_from(height).context("video height exceeds MP4 avc1 field")?;
    Ok(SampleEntry::Avc1(Avc1Box {
        visual: VisualSampleEntryFields {
            data_reference_index: VisualSampleEntryFields::DEFAULT_DATA_REFERENCE_INDEX,
            width,
            height,
            horizresolution: VisualSampleEntryFields::DEFAULT_HORIZRESOLUTION,
            vertresolution: VisualSampleEntryFields::DEFAULT_VERTRESOLUTION,
            frame_count: VisualSampleEntryFields::DEFAULT_FRAME_COUNT,
            compressorname: VisualSampleEntryFields::NULL_COMPRESSORNAME,
            depth: VisualSampleEntryFields::DEFAULT_DEPTH,
        },
        avcc_box: AvccBox {
            avc_profile_indication: avcc.profile,
            profile_compatibility: avcc.compatibility,
            avc_level_indication: avcc.level,
            length_size_minus_one: Uint::new(avcc.length_size_minus_one),
            sps_list: avcc.sps.clone(),
            pps_list: avcc.pps.clone(),
            chroma_format: None,
            bit_depth_luma_minus8: None,
            bit_depth_chroma_minus8: None,
            sps_ext_list: vec![],
        },
        unknown_boxes: vec![],
    }))
}

pub(crate) fn parse_avcc(data: &[u8]) -> anyhow::Result<AvccConfig> {
    if data.len() < 7 || data[0] != 1 {
        return Err(anyhow!("invalid AVCDecoderConfigurationRecord"));
    }
    let profile = data[1];
    let compatibility = data[2];
    let level = data[3];
    let length_size_minus_one = data[4] & 0x03;
    if length_size_minus_one != 3 {
        return Err(anyhow!("only 4-byte H264 NAL lengths are supported"));
    }
    let mut cursor = 6_usize;
    let sps_count = (data[5] & 0x1f) as usize;
    let mut sps = Vec::with_capacity(sps_count);
    for _ in 0..sps_count {
        sps.push(read_avcc_nal(data, &mut cursor)?);
    }
    let pps_count = *data
        .get(cursor)
        .ok_or_else(|| anyhow!("AVCC missing PPS count"))? as usize;
    cursor += 1;
    let mut pps = Vec::with_capacity(pps_count);
    for _ in 0..pps_count {
        pps.push(read_avcc_nal(data, &mut cursor)?);
    }
    if sps.is_empty() || pps.is_empty() {
        return Err(anyhow!("AVCC has no SPS/PPS"));
    }
    Ok(AvccConfig {
        profile,
        compatibility,
        level,
        length_size_minus_one,
        sps,
        pps,
    })
}

fn read_avcc_nal(data: &[u8], cursor: &mut usize) -> anyhow::Result<Vec<u8>> {
    let len_bytes = data
        .get(*cursor..*cursor + 2)
        .ok_or_else(|| anyhow!("AVCC truncated NAL length"))?;
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    *cursor += 2;
    let nal = data
        .get(*cursor..*cursor + len)
        .ok_or_else(|| anyhow!("AVCC truncated NAL"))?
        .to_vec();
    *cursor += len;
    Ok(nal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    const AVCC: [u8; 19] = [
        1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xe0, 0x1e, 1, 0, 4, 0x68, 0xee, 0x3c,
        0x80,
    ];

    fn parameters() -> VideoParameters {
        VideoParameters {
            rfc6381_codec: "avc1.42e01e".into(),
            pixel_dimensions: (640, 480),
            extra_data: AVCC.to_vec(),
            frame_rate: Some((1, 30)),
        }
    }

    fn frame(index: i64, keyframe: bool) -> Frame {
        Frame {
            data: Bytes::from(vec![0, 0, 0, 2, 0x65, index as u8]),
            timestamp: index * 3_000, // 30 fps at 90 kHz
            clock_rate: 90_000,
            keyframe,
            new_parameters: index == 0,
        }
    }

    #[test]
    fn a_segment_closes_on_the_keyframe_after_the_target() {
        // Keyframes every 3 frames (100 ms), target 100 ms: every keyframe
        // after the first closes a segment.
        let mut segmenter = Segmenter::new(Duration::from_millis(100)).unwrap();
        let parameters = parameters();
        let mut closed = Vec::new();
        for index in 0..7 {
            match segmenter
                .push(&frame(index, index % 3 == 0), Some(&parameters))
                .unwrap()
            {
                Pushed::Segment(segment) => closed.push(segment),
                Pushed::Nothing => {}
                Pushed::ParametersChanged => panic!("nothing changed"),
            }
        }
        assert_eq!(closed.len(), 2, "closed at frames 3 and 6");
        assert_eq!(closed[0].start_timestamp, 0);
        assert_eq!(closed[0].sequence, 0);
        assert_eq!(closed[1].start_timestamp, 9_000);
        // Three frames of 3000 ticks: the last one's duration comes from the
        // keyframe that closed the segment, not from a guess.
        assert_eq!(closed[0].duration_ticks, 9_000);
        assert!(!closed[0].bytes.is_empty());
    }

    #[test]
    fn frames_before_the_first_keyframe_are_dropped() {
        let mut segmenter = Segmenter::new(Duration::from_millis(100)).unwrap();
        let parameters = parameters();
        for index in 0..3 {
            let mut frame = frame(index, false);
            frame.new_parameters = false;
            assert!(matches!(
                segmenter.push(&frame, Some(&parameters)).unwrap(),
                Pushed::Nothing
            ));
        }
        assert_eq!(segmenter.pending_frames(), 0, "nothing decodable was kept");
    }

    #[test]
    fn a_stream_that_changes_shape_does_not_go_in_the_same_segment() {
        let mut segmenter = Segmenter::new(Duration::from_millis(100)).unwrap();
        let parameters = parameters();
        segmenter.push(&frame(0, true), Some(&parameters)).unwrap();

        let mut resized = parameters.clone();
        resized.pixel_dimensions = (1280, 720);
        let mut frame = frame(1, true);
        frame.new_parameters = true;
        assert!(matches!(
            segmenter.push(&frame, Some(&resized)).unwrap(),
            Pushed::ParametersChanged
        ));
        assert_eq!(
            segmenter.pending_frames(),
            1,
            "the frame that changed shape was not swallowed"
        );
    }

    #[test]
    fn flushing_closes_what_is_left() {
        let mut segmenter = Segmenter::new(Duration::from_secs(10)).unwrap();
        let parameters = parameters();
        for index in 0..4 {
            segmenter
                .push(&frame(index, index == 0), Some(&parameters))
                .unwrap();
        }
        let segment = segmenter.flush().unwrap().expect("a partial segment");
        assert_eq!(segment.start_timestamp, 0);
        assert!(segmenter.flush().unwrap().is_none(), "nothing left twice");
        assert!(
            !segmenter.init().unwrap().is_empty(),
            "an init segment describes the track"
        );
    }

    #[test]
    fn a_gop_longer_than_the_target_still_yields_one_segment_rather_than_none() {
        // Segments may only start on a keyframe. A camera whose GOP is longer
        // than the target must produce one long segment, never zero — losing
        // the video entirely is the worse failure.
        let mut segmenter = Segmenter::new(Duration::from_millis(100)).unwrap();
        let parameters = parameters();
        for index in 0..10 {
            assert!(matches!(
                segmenter
                    .push(&frame(index, index == 0), Some(&parameters))
                    .unwrap(),
                Pushed::Nothing
            ));
        }
        let segment = segmenter.flush().unwrap().expect("one long segment");
        assert_eq!(segment.start_timestamp, 0);
        assert_eq!(segment.sequence, 0);
    }

    #[test]
    fn frames_after_the_last_split_are_not_lost() {
        let mut segmenter = Segmenter::new(Duration::from_millis(100)).unwrap();
        let parameters = parameters();
        let mut closed = 0;
        for index in 0..5 {
            if let Pushed::Segment(_) = segmenter
                .push(&frame(index, index % 3 == 0), Some(&parameters))
                .unwrap()
            {
                closed += 1;
            }
        }
        assert_eq!(closed, 1, "closed once, at frame 3");
        let tail = segmenter.flush().unwrap().expect("frames 3 and 4 remain");
        assert_eq!(tail.start_timestamp, 9_000);
    }

    #[test]
    fn parses_avcc_sps_pps() {
        let avcc = parse_avcc(&AVCC).unwrap();
        assert_eq!(avcc.profile, 0x42);
        assert_eq!(avcc.sps.len(), 1);
        assert_eq!(avcc.pps.len(), 1);
        assert_eq!(avcc.length_size_minus_one, 3);
    }

    #[test]
    fn ticks_convert_without_overflowing_at_long_durations() {
        let clock_rate = NonZeroU32::new(90_000).unwrap();
        assert_eq!(ticks_to_ms(90_000, clock_rate), 1_000);
        // Twelve hours of 90 kHz ticks overflow a u64 multiplication done
        // naively, which is why this goes through u128.
        assert_eq!(ticks_to_ms(90_000 * 3600 * 12, clock_rate), 43_200_000);
    }
}
